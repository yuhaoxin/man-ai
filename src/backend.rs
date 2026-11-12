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
    ChatCompletionRequestUserMessage, CreateChatCompletionRequestArgs,
    CreateChatCompletionResponse, FinishReason,
};

use async_openai::{Chat, Client};

use crate::config::Config;

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
    pub finish_reason: Option<String>,
}

/// 已规范化的后端：endpoint、model、密钥与 HTTP client。
pub struct Backend {
    client: Client<OpenAIConfig>,
    /// 配置原值；缓存键里用它的规范化形式（§9.1）。
    endpoint: String,
    model: String,
    max_tokens: Option<u32>,
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

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs_f64(config.timeout_seconds))
            .redirect(reqwest::redirect::Policy::none());
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
            max_tokens: config.max_tokens,
        })
    }

    /// 缓存键需要的后端身份：端点配置原值与模型名。
    pub fn key_parts(&self) -> (&str, &str) {
        (&self.endpoint, &self.model)
    }

    /// 单次尝试（§7.1、§10.3）。
    #[allow(deprecated)]
    pub async fn post(&self, system: &str, user: &str) -> Result<Completion, Failure> {
        let messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage::from(system).into(),
            ChatCompletionRequestUserMessage::from(user).into(),
        ];
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(&self.model)
            .messages(messages)
            .temperature(0.0_f32)
            .stream(false);
        if let Some(max_tokens) = self.max_tokens {
            builder.max_tokens(max_tokens);
        }
        let request = builder
            .build()
            .map_err(|_| Failure::new(Reason::BackendError, false))?;

        let response = Chat::new(&self.client)
            .create(request)
            .await
            .map_err(|error| self.classify(error))?;
        completion(response)
    }

    fn classify(&self, error: OpenAIError) -> Failure {
        match error {
            OpenAIError::Reqwest(error) => {
                if error.is_timeout() {
                    Failure::new(Reason::Timeout, false)
                } else if error.is_connect() {
                    Failure::new(Reason::Unreachable, true)
                } else {
                    // 响应体残缺、连接中断等：可重试一次。
                    Failure::new(Reason::BackendError, true)
                }
            }
            OpenAIError::ApiError(response) => {
                let status = response.status_code;
                let retryable = status.as_u16() == 429 || status.is_server_error();
                Failure::new(Reason::BackendError, retryable)
                    .with_diagnostic(http_diagnostic(status, &response.api_error))
            }
            // 非 JSON、结构不符、空响应。
            _ => Failure::new(Reason::BackendError, true),
        }
    }
}

fn not_configured(item: &str) -> Failure {
    Failure::new(Reason::NotConfigured, false).with_diagnostic(format!(
        "backend not configured (missing {item}); not translating"
    ))
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

fn completion(response: CreateChatCompletionResponse) -> Result<Completion, Failure> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| Failure::new(Reason::BackendError, true))?;
    let content = choice
        .message
        .content
        .ok_or_else(|| Failure::new(Reason::BackendError, true))?;
    Ok(Completion {
        content,
        finish_reason: choice.finish_reason.map(finish_name),
    })
}

fn finish_name(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::FunctionCall => "function_call",
    }
    .to_string()
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
}
