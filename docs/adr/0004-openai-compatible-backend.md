# 0004 后端契约：OpenAI 兼容、Bearer、固定参数

- 状态：接受
- 日期：2026-11-12
- 决策来源：[#8](https://github.com/yuhaoxin/man-ai/issues/8)、[#5](https://github.com/yuhaoxin/man-ai/issues/5)、[#15](https://github.com/yuhaoxin/man-ai/issues/15)

## 背景

用户既要云端服务，也要本地推理端点（常见形态是 `http://127.0.0.1:11434/v1`）。翻译需要可重复的整块校验、精确的错误分类与受控的重试，而不是 agent 编排或多供应商路由。

候选方案：

1. 各供应商原生 SDK：能力最全，但每接一家就多一套认证、错误与参数差异。
2. 通用多供应商抽象（`rig-core`/`genai` 等）：面向 agent 与会话，依赖重，且会吞掉原始状态码与 `error.code`。
3. OpenAI 兼容协议 + 自写薄客户端：所有兼容端点一份代码，请求参数与错误映射完全可控。
4. 内置默认端点：开箱即用，但把厂商选择与密钥策略绑进工具。

## 决策

采用方案 3。

- 固定 `POST {endpoint}/chat/completions`，`Authorization: Bearer <key>`；不自动补 `/v1`、不剥路径，只去掉尾部 `/`。
- `temperature=0`、`stream=false` 固定；`max_tokens` 可选，字段名保持 `max_tokens`。
- 无内置默认端点；`endpoint`、`model` 必填，远端端点还必须有密钥。密钥只来自配置文件 `api_key` 或 `api_key_env` 指向的变量。
- `http://` 与 `https://` 都接受；HTTPS 只信系统信任库，不提供跳过校验的开关，不支持自签证书与 Azure 专有认证。
- 显式 `proxy` 完全取代标准代理变量；未设置时由 HTTP 客户端读取 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`/`NO_PROXY`。
- 每次尝试有单次超时（默认 30 秒）；连接失败、DNS 失败、5xx、429 重试一次、间隔 500ms；超时与确定性 4xx 不重试。
- 失败原因固定五项：`not configured`、`model unreachable`、`timeout`、`backend error`、`interrupted`。诊断只写状态码与 `error.code`/`error.type`，不写 `error.message` 与原文。
- 实现复用 `async-openai 0.43`（`chat-completion` + `rustls` + `middleware`），用 `reqwest 0.13` 构造带超时与代理的 Client；`with_http_service(ReqwestService)` 跳过库内置的 3 次重试，重试包装自写。

## 后果

- 任何 OpenAI 兼容端点都可以接入，包括本地端点；空密钥对本地端点合法，请求头为裸 `Bearer `。
- 后端响应不做流式（`stream=false`），首屏收益来自 man-ai 侧的按块写出；若将来必须支持 SSE，需要换传输层或加流式解析，不在 v1。
- 错误映射绑定 `async-openai` 的错误类型；`0.43.x` 升级需要重新核对状态码、`error.code` 与 `max_tokens` 字段。
- 没有单页成本上限与按 token 计价；成本只能靠缓存、`concurrency`、`max_tokens` 间接约束。
- 不支持系统钥匙串、自定义 CA、Azure `api-key` 头与 `api-version` 查询参数。
- 请求参数固定使缓存键稳定；任何固定参数变化都必须递增 `PROMPT_VERSION`。
