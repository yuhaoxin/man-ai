//! OpenAI 兼容后端：单次请求、失败分类与诊断（规格 §10.3–§10.5）。
//!
//! 重试与回退不在这里：`post` 恰好发一次请求，由 `translate` 决定是否重试。
//! `with_http_service(ReqwestService)` 是必须的，否则 async-openai 会自己重试 3 次。
use std::time::Duration;

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::middleware::ReqwestService;
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestUserMessage, ChatCompletionStreamOptions,
    CreateChatCompletionRequest, CreateChatCompletionRequestArgs, FinishReason, ReasoningEffort,
};

use async_openai::{Chat, Client};
use futures::StreamExt;

use crate::config::{Config, ReasoningEffort as ConfigEffort};

/// 五个固定原因（§10.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    NotConfigured,
    Unreachable,
    Timeout,
    BackendError,
    Interrupted,
}

impl Reason {
    /// 页首/尾行与 stderr 里出现的原因文案（英文）。
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotConfigured => "not configured",
            Reason::Unreachable => "model unreachable",
            Reason::Timeout => "timeout",
            Reason::BackendError => "backend error",
            Reason::Interrupted => "interrupted",
        }
    }
}

/// 一次请求失败：原因、是否可重试，以及最多一行的诊断（不含原文与密钥）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: Reason,
    pub retryable: bool,
    /// 供调用方写到 stderr 的一行（§10.5）；同一条只应输出一次。
    pub diagnostic: Option<String>,
}

impl Failure {
    pub fn new(reason: Reason, retryable: bool) -> Self {
        Self {
            reason,
            retryable,
            diagnostic: None,
        }
    }

    fn with_diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostic = Some(diagnostic.into());
        self
    }
}

/// 一次成功请求的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    /// 流中出现过 `finish_reason == "stop"`（§7.4 采用条件之一）。
    pub stopped: bool,
    /// 流中最后一个非空 usage 块（§7.5）；缺失即未知，不按 0 补。
    pub usage: Option<Usage>,
}

/// 一次请求的 usage 原始值（§8.3 三口径的输入；cached 缺失即未知）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// `prompt_tokens_details.cached_tokens`；缺失即未知。
    pub cached_tokens: Option<u32>,
}

impl Usage {
    fn from_completion(usage: async_openai::types::chat::CompletionUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
        }
    }
}

/// 已规范化的后端：endpoint、model、密钥与 HTTP client。
pub struct Backend {
    client: Client<OpenAIConfig>,
    /// 配置原值；缓存键里用它的规范化形式（§9.1）。
    endpoint: String,
    model: String,
    reasoning_effort: Option<ConfigEffort>,
    max_tokens: Option<u32>,
    /// 单次尝试总时限（§5.4）：发请求到流读完，由自有计时器执行。
    timeout: Duration,
}

impl Backend {
    /// 按配置构造。缺必填项返回 `NotConfigured`，诊断里带缺失的键名。
    pub fn from_config(config: &Config) -> Result<Self, Failure> {
        let endpoint = config
            .endpoint
            .as_deref()
            .ok_or_else(|| not_configured("endpoint"))?;
        let model = config
            .model
            .as_deref()
            .ok_or_else(|| not_configured("model"))?;
        let url = reqwest::Url::parse(endpoint).map_err(|_| not_configured("endpoint"))?;
        let key = match resolve_key(config) {
            Some(key) => key,
            None if is_local(&url) => String::new(),
            None => return Err(not_configured("api_key")),
        };

        // 不设 client 级超时：它先于自有计时器触发，且与断连在传输层不可区分（§5.4）。
        let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        if let Some(proxy) = &config.proxy {
            let proxy = reqwest::Proxy::all(proxy).map_err(|_| {
                Failure::new(Reason::NotConfigured, false)
                    .with_diagnostic("backend not configured (invalid proxy); not translating")
            })?;
            builder = builder.proxy(proxy);
        }
        let http = builder.build().map_err(|_| {
            Failure::new(Reason::NotConfigured, false)
                .with_diagnostic("backend not configured (http client); not translating")
        })?;

        let openai = OpenAIConfig::new()
            .with_api_base(endpoint.trim_end_matches('/'))
            .with_api_key(key);
        let client = Client::with_config(openai).with_http_service(ReqwestService::new(http));
        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
            model: model.to_string(),
            reasoning_effort: config.reasoning_effort,
            max_tokens: config.max_tokens,
            timeout: Duration::from_secs_f64(config.timeout_seconds),
        })
    }

    /// 缓存键需要的后端身份：端点配置原值、模型名与思考强度。
    pub fn key_parts(&self) -> (&str, &str, &'static str) {
        let effort = self.reasoning_effort.map_or("off", ConfigEffort::as_str);
        (&self.endpoint, &self.model, effort)
    }

    /// 单次流式尝试（§7.1、§10.3）：固定 `stream: true` + `include_usage: true`。
    ///
    /// `on_delta` 随每个内容片段到达调用；自有计时器包住发请求到流读完
    /// （§5.4），触发即 `timeout`、不重试。
    #[allow(deprecated)]
    pub async fn post(
        &self,
        system: &str,
        user: &str,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<Completion, Failure> {
        let messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage::from(system).into(),
            ChatCompletionRequestUserMessage::from(user).into(),
        ];
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(&self.model)
            .messages(messages)
            .temperature(0.0_f32)
            .stream(true)
            .stream_options(ChatCompletionStreamOptions {
                include_usage: Some(true),
                ..Default::default()
            });
        if let Some(effort) = self.reasoning_effort {
            builder.reasoning_effort(openai_effort(effort));
        }
        if let Some(max_tokens) = self.max_tokens {
            builder.max_tokens(max_tokens);
        }
        let request = builder
            .build()
            .map_err(|_| Failure::new(Reason::BackendError, false))?;

        let attempt = self.stream(request, on_delta);
        tokio::time::timeout(self.timeout, attempt)
            .await
            .map_err(|_| Failure::new(Reason::Timeout, false))?
    }

    /// 消费流到读完：累积 `delta.content`、记录 finish_reason 与最后一个非空
    /// usage 块（§7.3、§7.5）；reasoning 等其余字段一律忽略。
    async fn stream(
        &self,
        request: CreateChatCompletionRequest,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<Completion, Failure> {
        let mut stream = Chat::new(&self.client)
            .create_stream(request)
            .await
            .map_err(|error| self.classify(error))?;
        let mut completion = Completion {
            content: String::new(),
            stopped: false,
            usage: None,
        };
        while let Some(chunk) = stream.next().await {
            // 2xx 之后的流错误（error 帧、断连、帧解码失败）一律可重试
            // `backend error`（§7.5）。
            let chunk = chunk.map_err(|error| self.classify(error))?;
            if let Some(usage) = chunk.usage {
                completion.usage = Some(Usage::from_completion(usage));
            }
            for choice in chunk.choices {
                if let Some(reason) = choice.finish_reason {
                    completion.stopped = reason == FinishReason::Stop;
                }
                if let Some(text) = choice.delta.content {
                    completion.content.push_str(&text);
                    on_delta(&text);
                }
            }
        }
        Ok(completion)
    }

    fn classify(&self, error: OpenAIError) -> Failure {
        match error {
            OpenAIError::Reqwest(error) if error.is_connect() => {
                Failure::new(Reason::Unreachable, true)
            }
            OpenAIError::ApiError(response) => {
                let status = response.status_code;
                let retryable = status.as_u16() == 429 || status.is_server_error();
                Failure::new(Reason::BackendError, retryable)
                    .with_diagnostic(http_diagnostic(status, &response.api_error))
            }
            // 非 JSON、结构不符、空响应与流内错误：可重试一次。
            // 超时不在这里：client 不设 `.timeout`，超时由自有计时器判定（§5.4）。
            _ => Failure::new(Reason::BackendError, true),
        }
    }
}

fn not_configured(item: &str) -> Failure {
    Failure::new(Reason::NotConfigured, false).with_diagnostic(format!(
        "backend not configured (missing {item}); not translating"
    ))
}

fn openai_effort(effort: ConfigEffort) -> ReasoningEffort {
    match effort {
        ConfigEffort::None => ReasoningEffort::None,
        ConfigEffort::Minimal => ReasoningEffort::Minimal,
        ConfigEffort::Low => ReasoningEffort::Low,
        ConfigEffort::Medium => ReasoningEffort::Medium,
        ConfigEffort::High => ReasoningEffort::High,
        ConfigEffort::Xhigh => ReasoningEffort::Xhigh,
        ConfigEffort::Max => ReasoningEffort::Max,
    }
}

fn resolve_key(config: &Config) -> Option<String> {
    if let Some(key) = config.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Some(key.to_string());
    }
    let name = config.api_key_env.as_deref()?;
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// 本地端点允许空密钥（§10.2、§10.3）。
fn is_local(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
    })
}

fn http_diagnostic(status: reqwest::StatusCode, error: &async_openai::error::ApiError) -> String {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return "backend returned HTTP 401; check api_key / api_key_env".to_string();
    }
    let mut line = format!("backend returned HTTP {}", status.as_u16());
    if let Some(code) = &error.code {
        line.push_str(&format!("; error.code={code}"));
    }
    if let Some(kind) = &error.r#type {
        line.push_str(&format!("; error.type={kind}"));
    }
    line
}


#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn 原因文案为英文() {
        assert_eq!(Reason::NotConfigured.as_str(), "not configured");
        assert_eq!(Reason::Unreachable.as_str(), "model unreachable");
        assert_eq!(Reason::Timeout.as_str(), "timeout");
        assert_eq!(Reason::BackendError.as_str(), "backend error");
        assert_eq!(Reason::Interrupted.as_str(), "interrupted");
    }

    #[test]
    fn 缺_endpoint_或_model_是未配置() {
        let mut cfg = config();
        let failure = Backend::from_config(&cfg).err().unwrap();
        assert_eq!(failure.reason, Reason::NotConfigured);
        assert!(!failure.retryable);
        assert_eq!(
            failure.diagnostic.as_deref(),
            Some("backend not configured (missing endpoint); not translating")
        );

        cfg.endpoint = Some("https://api.example.com/v1".to_string());
        let failure = Backend::from_config(&cfg).err().unwrap();
        assert_eq!(
            failure.diagnostic.as_deref(),
            Some("backend not configured (missing model); not translating")
        );
    }

    #[test]
    fn 远端缺密钥是未配置() {
        let mut cfg = config();
        cfg.endpoint = Some("https://api.example.com/v1".to_string());
        cfg.model = Some("m".to_string());
        let failure = Backend::from_config(&cfg).err().unwrap();
        assert_eq!(
            failure.diagnostic.as_deref(),
            Some("backend not configured (missing api_key); not translating")
        );
    }

    #[test]
    fn 本地端点允许空密钥() {
        for endpoint in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
        ] {
            let mut cfg = config();
            cfg.endpoint = Some(endpoint.to_string());
            cfg.model = Some("m".to_string());
            assert!(Backend::from_config(&cfg).is_ok(), "{endpoint}");
        }
    }

    #[test]
    fn 代理只接受可解析的地址() {
        let mut cfg = config();
        cfg.endpoint = Some("http://127.0.0.1:8080".to_string());
        cfg.model = Some("m".to_string());
        cfg.proxy = Some("socks5://127.0.0.1:1080".to_string());
        assert!(Backend::from_config(&cfg).is_ok());
    }

    /// 起一个一次性 SSE 端点：接受一个连接，写入 `body` 后关闭。
    /// 返回 endpoint；`post` 的整个流程都在同一个 runtime 里跑完。
    async fn sse_endpoint(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        endpoint
    }

    fn local_backend(endpoint: &str) -> Backend {
        let mut cfg = config();
        cfg.endpoint = Some(endpoint.to_string());
        cfg.model = Some("m".to_string());
        Backend::from_config(&cfg).unwrap()
    }

    fn chunk_frame(content: &str, finish: Option<&str>, usage: &str) -> String {
        let mut frame = format!(
            "{{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\
             \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}\
             {}}}]",
            finish.map(|f| format!(",\"finish_reason\":\"{f}\"")).unwrap_or_default()
        );
        if !usage.is_empty() {
            frame.push_str(&format!(",\"usage\":{usage}"));
        }
        frame.push('}');
        format!("data: {frame}\n\n")
    }

    #[test]
    fn 流式累积且_usage_取最后一个非空块() {
        // 首帧挂 usage（DeepSeek 形态），末尾独立 usage 块覆盖它（OpenAI 形态）。
        let usage_frame = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,",
            "\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":130,",
            "\"completion_tokens\":9,\"total_tokens\":139,",
            "\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\n",
        );
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{}{}{}{}data: [DONE]\n\n",
            chunk_frame("He", None, "{\"prompt_tokens\":120,\"completion_tokens\":7,\"total_tokens\":127,\"prompt_tokens_details\":{\"cached_tokens\":64}}"),
            chunk_frame("llo", None, ""),
            chunk_frame("", Some("stop"), ""),
            usage_frame,
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let endpoint = sse_endpoint(body).await;
            let backend = local_backend(&endpoint);
            let mut seen = String::new();
            let completion = backend
                .post("sys", "user", &mut |chunk| seen.push_str(chunk))
                .await
                .unwrap();
            assert_eq!(completion.content, "Hello");
            assert!(completion.stopped);
            assert_eq!(
                completion.usage,
                Some(Usage {
                    prompt_tokens: 130,
                    completion_tokens: 9,
                    cached_tokens: Some(64),
                })
            );
            // 增量随帧到达调用方（DeepSeek 挂在内容块、OpenRouter 总附带都覆盖）。
            assert_eq!(seen, "Hello");
        });
    }

    #[test]
    fn usage_缺失即未知且中途_error_帧可重试() {
        let no_usage = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{}{}data: [DONE]\n\n",
            chunk_frame("ok", None, ""),
            chunk_frame("", Some("stop"), ""),
        );

        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let endpoint = sse_endpoint(no_usage).await;
            let backend = local_backend(&endpoint);
            let completion = backend.post("sys", "user", &mut |_| {}).await.unwrap();
            assert_eq!(completion.usage, None, "缺失即未知，不按 0 补");
            assert!(completion.stopped);
        });

        let error_frame = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{}data: {{\"error\":{{\"message\":\"boom\",\"type\":\"server_error\"}}}}\n\ndata: [DONE]\n\n",
            chunk_frame("半截", None, ""),
        );

        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let endpoint = sse_endpoint(error_frame).await;
            let backend = local_backend(&endpoint);
            let failure = backend.post("sys", "user", &mut |_| {}).await.unwrap_err();
            // 中途 error 帧表现为 JSONDeserialize：2xx 之后一律可重试 backend error。
            assert_eq!(failure.reason, Reason::BackendError);
            assert!(failure.retryable);
        });
    }
}
