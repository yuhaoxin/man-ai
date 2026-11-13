# 事实核查：OpenAI 兼容流式 chat/completions 的 usage 与 cached token

- 日期：2026-11-14
- 票据：[OpenAI 兼容流式 usage 与 cached token 事实核查](https://github.com/yuhaoxin/man-ai/issues/38)（地图：[Markdown 流式翻译与终端渲染改造](https://github.com/yuhaoxin/man-ai/issues/35)）
- 核查对象：`async-openai 0.43.0`（`Cargo.lock` 锁定；特性 `chat-completion`、`rustls`、`middleware`）、`reqwest 0.13.5`、OpenAI Chat Completions 流式协议
- 标注约定：
  - **文档**：OpenAI 官方 API 参考、指南或 OpenAPI 规范
  - **源码**：async-openai 0.43.0 或 reqwest 0.13.5 源码，给出 tag 与行号
  - **实测**：本地假 SSE 服务器 + async-openai 0.43.0 客户端跑出的原始输出
  - **推导**：由多条文档/源码拼接得出，官方没有针对该场景的原话
  - **未确认**：只有间接依据或本次未覆盖

---

## 0. 结论速查

| 问题 | 结论 | 依据 |
|---|---|---|
| 如何要求流式 usage | 请求带 `stream: true` 与 `stream_options.include_usage: true`；async-openai 的 `Chat::create_stream` 会强制把 `stream` 置为 `true` | 文档 O1、O3；源码 C1、C3 |
| usage 块在哪里 | 独立块，位于其余所有块之后、`data: [DONE]` 之前（“其余块 `usage` 为 `null`” + “additional chunk” 两条合起来推出）；该块 `choices` 为空数组 | 文档 O1、O3 |
| 其他块的 usage 值 | 每块都带 `usage` 字段，值为 `null`；async-openai 把 `null` 与字段缺失都反序列化成 `None` | 文档 O1；源码 C2 |
| 缓存输入字段 | `usage.prompt_tokens_details.cached_tokens`，prompt 中被缓存命中的 token 数 | 文档 O1、O4 |
| cached 与 prompt 的关系 | 子集：`prompt_tokens` 含已缓存与未缓存的输入 token | 文档 O7、O8；推导 O4 |
| 非缓存输入 | `prompt_tokens - cached_tokens - cache_write_tokens`（缺失时的处理见 §5） | 推导：文档 O1、O4；旁证 D2 |
| 输出 token | `completion_tokens`；推理模型的 `completion_tokens_details.reasoning_tokens` 也计入其中 | 文档 O1、O3 |
| 端点不返回 usage | 所有块 `usage` 为 `None`，流仍正常结束；仅凭 usage 无法判断请求是否完整 | 文档 O2；实测 P2、P3、P5 |
| 中途错误帧 | data 帧可换成带 `error` 对象的 JSON；async-openai 抛 `JSONDeserialize`，原始 JSON 在该错误的字符串里 | 文档 O3；实测 P6 |
| 截断 | 没有 `[DONE]`、甚至没有 `finish_reason` 也会正常结束；半个 SSE 帧被静默丢弃 | 文档 O3；实测 P3、P4、P5 |
| 超时 | async-openai 把传输层错误压成 `StreamError(EventStream(...))`，超时与断连、解码失败不可区分 | 实测 P7、P8 |
| 总时限 | reqwest `timeout` 覆盖到响应体读完，会杀死长流；`read_timeout` 每次读取后重置 | 文档 Q1；实测 P7、P9 |
| 页面级指标 | 只有每请求一份 usage，整页必须由 man-ai 自己累加；服务端不提供跨请求聚合 | 文档 O1；推导 |

---

## 1. 版本与复现方法

### 1.1 版本

| 项 | 值 |
|---|---|
| async-openai | 0.43.0（`Cargo.lock` 校验和 `2c30702b…`） |
| reqwest | 0.13.5 |
| crate 源码 tag | [`async-openai-v0.43.0`](https://github.com/64bit/async-openai/tree/async-openai-v0.43.0)，tag 指向 `e4daebb3ef3f4b89df9fbab5fb319bff077dcfce`；仓库是 workspace，源码在 `async-openai/` 子目录 |
| 实测环境 | macOS 26.6.2 arm64；`cargo run` 单进程内起 axum 0.8.9 假 SSE 服务器 |

async-openai 的 `chat-completion` 特性会启用 `_api`，其中已含 `eventsource-stream` 依赖，流式能力不需要额外特性（[`Cargo.toml` 第 86–93 行](https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/Cargo.toml#L86-L93)）。

### 1.2 实测方法

本地 axum 服务器在 `POST /v1/chat/completions` 上按请求里的 `model` 字段切换场景，返回 `text/event-stream`。客户端与 man-ai 的构造方式一致：`Client::with_config(openai).with_http_service(ReqwestService::new(http))`，请求由 `CreateChatCompletionRequestArgs` 加 `stream_options.include_usage = true` 生成；逐 item 打印 `choices.len()`、`delta.content`、`finish_reason`、`usage`。

| 场景 | 服务器行为 |
|---|---|
| `normal` | 内容块 ×2 → `finish_reason: stop` 块 → `choices: []` 的 usage 块 → `data: [DONE]` |
| `nousage` | 内容块 → `finish_reason` 块 → `[DONE]`，不返回 usage |
| `nodone` | 内容块 → `finish_reason` 块，直接关闭连接，不发 `[DONE]` |
| `partial` | 内容块 → 半个 SSE 帧（`data: {"id":"chatcmpl-1","cho`）→ 关闭 |
| `cut` | 两个完整内容块，没有 `finish_reason`、没有 usage、没有 `[DONE]`，关闭 |
| `rider` | 内容块 → 同一块里同时带 `finish_reason` 与 usage（DeepSeek 形态）→ `[DONE]` |
| `errorframe` | 内容块 → `data: {"error":{"message":"boom","type":"server_error"}}` → `[DONE]` |
| `slow` | 内容块 → 停 2 s → 内容块 → `[DONE]` |
| `drip` | 每 200 ms 一个内容块，共 8 个，总时长约 1.6 s |

完整输出见 §8。

---

## 2. 请求侧：怎样要求流式与 usage

### 2.1 协议字段

- `stream: true` 打开 SSE；官方对这一路径的定位是“Streaming responses lets you start printing or processing the beginning of the model's output while it continues generating the full response.”（文档 O5）。`stream_options` 的官方说明是“Options for streaming response. Only set this when you set `stream: true`”（文档 O1）。
- `stream_options.include_usage: true`：官方原文为“If set, an additional chunk will be streamed before the `data: [DONE]` message. The `usage` field on this chunk shows the token usage statistics for the entire request, and the `choices` field will always be an empty array.”以及“All other chunks will also include a `usage` field, but with a null value.”（文档 O1；同文见规范 O3 第 44748–44780 行）。
- `stream_options.include_obfuscation`：默认开启，会在流块上附带随机字符串以归一化载荷大小（文档 O1）。async-openai 不主动设置该字段（源码 C3）。

兼容端点对 `stream_options` 的接受程度不同：DeepSeek 要求 `stream_options` 必须与 `stream: true` 同时出现，否则返回 400（文档 D1）；OpenRouter 则声明 Chat Completions 上的 `stream_options.include_usage` 已废弃且无效（文档 D2）。

### 2.2 async-openai 0.43 的入口

- `Chat::create_stream(request) -> Result<ChatCompletionResponseStream, OpenAIError>`：若 `request.stream == Some(false)` 直接返回 `InvalidArgument`，否则强制写入 `Some(true)` 再 `POST /chat/completions`（源码 C1，第 75–92 行）。
- 返回类型是 `Pin<Box<dyn Stream<Item = Result<CreateChatCompletionStreamResponse, OpenAIError>> + Send>>`（源码 C4，第 1281–1283 行）。
- `Chat::create`（非流式）在 `stream == Some(true)` 时同样直接报错，两条路径不能混用（[`src/chat.rs` 第 44–53 行](https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/chat.rs#L44-L53)）。
- 请求类型里 `stream` 与 `stream_options` 都是 `Option`，序列化时 `skip_serializing_if = "Option::is_none"`（[`chat_.rs` 第 987–1051 行](https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L987-L1051)）。

### 2.3 重试边界

默认客户端会套 `OpenAIRetryLayer`，`should_retry` 只重试 429、5xx 和 `reqwest` 的连接错误（源码 C8，第 58–67 行）；重试发生在拿到响应头之前，流一旦开始就没有重试。man-ai 现有实现用 `ReqwestService::new(http)` 绕过该层，因此 429 与 5xx 会原样返回给调用方（[`src/backend.rs`](../src/backend.rs) 第 4 行的注释给出了同样的结论）。

---

## 3. 响应侧：块结构与顺序

### 3.1 文档给出的结构

`chat.completion.chunk` 的字段：`id`、`object: "chat.completion.chunk"`、`created`、`model`、`choices[]`、`usage`（仅 `include_usage` 时出现）、`service_tier`、`system_fingerprint`、`obfuscation`、`moderation`（文档 O2；规范 O3 第 46643–46780 行）。

`choices[]` 每项含 `index`、`delta`、`finish_reason`、`logprobs`。`finish_reason` 取值 `stop` / `length` / `tool_calls` / `content_filter` / `function_call`，未结束时为 `null`（文档 O2）。

### 3.2 顺序

官方对顺序的描述分散在两处：`include_usage` 的说明写“an additional chunk will be streamed before the `data: [DONE]` message … the `choices` field will always be an empty array”，同时写“All other chunks will also include a `usage` field, but with a null value”（文档 O1，规范 O3 第 44748–44780 行）。两条合起来给出的位置是：它是额外的一块，排在其余所有块之后、`[DONE]` 之前；其余块（含带 `finish_reason` 的最后生成块）的 `usage` 都是 `null`。

没有官方文字逐帧列出“`finish_reason` 块 → usage 块 → `[DONE]`”这一串；未开 `include_usage` 的官方示例只有：`role: assistant` 的空 delta 块 → 若干内容块 → `delta: {}` 且 `finish_reason: "stop"` 的块 → `data: [DONE]`（文档 O1 的 Streaming 示例）。

合起来，开启 `include_usage` 后可观察的顺序为：

```text
content delta × N
chunk(delta: {}, finish_reason: "stop", choices.len() == 1, usage: null)
chunk(choices: [], usage: {...})
data: [DONE]
```

实测 `normal` 场景就是这四类块，async-openai 逐个 yield，空 `choices` 的 usage 块作为独立 item 出现（实测 P1）。判断结束应看 `finish_reason` 与 usage 是否到齐；官方没有逐帧声明 usage 块与 `finish_reason` 块的相对位置。

### 3.3 async-openai 的解析行为

- `usage` 是 `Option<CompletionUsage>`，`null` 与缺失都得到 `None`，无法区分“遵守了 include_usage 但本块无 usage”与“端点不支持该参数”（源码 C2）。
- `choices` 为空数组不报错，usage 块可正常解析（实测 P1）。
- `finish_reason` 缺失不阻止解析，async-openai 不做“必须有 finish_reason”的校验（源码 C2；实测 P5）。
- `id`、`object`、`created`、`model`、`choices` 是必填字段，缺失即 `JSONDeserialize`（源码 C2；实测 P6 的报错是 `missing field \`id\``）。只返回 `{choices:[...]}` 的精简端点会在这里直接失败。

---

## 4. usage 字段定义

### 4.1 字段表（文档 O1、O3 第 45226–45320 行；async-openai 对应类型见源码 C4、C5）

| 字段 | 类型 | 官方说明 | 备注 |
|---|---|---|---|
| `prompt_tokens` | integer | Number of tokens in the prompt | 已含缓存命中部分 |
| `completion_tokens` | integer | Number of tokens in the generated completion | 推理模型的 reasoning token 计入 |
| `total_tokens` | integer | Total number of tokens used in the request (prompt + completion) | 即前两者之和 |
| `prompt_tokens_details.cached_tokens` | integer，默认 0 | Cached tokens present in the prompt | 缺失 ≠ 0 |
| `prompt_tokens_details.cache_write_tokens` | integer，默认 0 | The unadjusted number of prompt tokens written to cache | 只在有缓存写入计费的模型出现 |
| `prompt_tokens_details.text_tokens` / `image_tokens` / `audio_tokens` | integer | 输入按模态拆分 | 与 `cached_tokens` 不是同一维度 |
| `completion_tokens_details.reasoning_tokens` | integer，默认 0 | Tokens generated by the model for reasoning | 计入 `completion_tokens` |
| `completion_tokens_details.accepted_prediction_tokens` / `rejected_prediction_tokens` | integer，默认 0 | Predicted Outputs 的接受/拒绝数 | 官方注明 rejected 与 reasoning 一样仍计入 completion |

### 4.2 cached_tokens 是 prompt_tokens 的子集

直接证据：OpenAI 官方 cookbook 写“All requests … will display a `cached_tokens` field of the `usage.prompt_tokens_details` chat completions object indicating how many of the prompt tokens were a cache hit. For requests under 1024 tokens, `cached_tokens` will be zero.”（文档 O7）。同仓库的 Prompt Caching 201 写“how many of the prompt tokens were cached”（文档 O8）。

结构证据：Responses API 的定价示例把普通输入算成 `input_tokens - cached_tokens - cache_write_tokens`，并注明“Do not add `cache_write_tokens` again after counting its two components.”（文档 O4）。Chat Completions 的字段命名与语义一致，`cache_write_tokens` 的定义也明确是“prompt tokens written to cache”（文档 O1）。

**推导**：Chat Completions 上可靠的拆分是

```text
缓存输入   = prompt_tokens_details.cached_tokens
缓存写入   = prompt_tokens_details.cache_write_tokens（缺失按 0，但仅当端点声明支持该维度）
非缓存输入 = prompt_tokens - cached_tokens - cache_write_tokens
输出       = completion_tokens
```

官方没有针对 Chat Completions 写出这行减法，属于按字段定义拼接的推导。第三方 OpenRouter 文档给出同构说明与示例：`prompt_tokens: 194`、`cached_tokens: 0`、`cache_write_tokens: 100`，并解释“cached_tokens is the number of tokens that were read from the cache. cache_write_tokens is the number of tokens that were written to the cache”（文档 D2）。

### 4.3 reasoning token 与输出

`completion_tokens_details.rejected_prediction_tokens` 的说明明确写“like reasoning tokens, these tokens are still counted in the total completion tokens for purposes of billing, output, and context window limits.”（规范 O3 第 45263 行起），因此输出 token 直接取 `completion_tokens`。若想额外展示“可见文本 token”，只能另算 `completion_tokens - reasoning_tokens` 并明确标为可见部分，不能把它当成计费输出。

---

## 5. 缺失、异常与兼容端点的偏差

### 5.1 usage 缺失

- OpenAI 文档警告：流被中断或取消时可能收不到最终 usage 块（文档 O1、O2）。
- 端点忽略或不支持 `stream_options` 时（实测用忽略该参数的假服务器），`usage` 全部为 `None`，流照常以 `[DONE]` 结束，async-openai 不报错（实测 P2）。
- 缺失不能当成 0：0 表示“确认没有缓存命中”，缺失表示“不知道该请求的 token 用量”。OpenAI 在 Agents API 文档里也写“Missing usage does not mean zero usage.”（文档 O9）。

### 5.2 兼容端点的文档差异

| 端点 | 与官方 OpenAI 的差异 | 依据 |
|---|---|---|
| DeepSeek | 不发独立的 usage 块，usage 挂在最后一个内容块上：该块 `choices` 长度 1、无新内容、`finish_reason` 非空；不开 `include_usage` 时，除最后一块外其余块没有 `usage` 字段 | 文档 D1 |
| DeepSeek | 额外提供 `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`，并明确 `prompt_tokens = hit + miss`、`prompt_tokens_details.cached_tokens` 与 hit 相同 | 文档 D1 |
| OpenRouter | Chat Completions 的 `stream_options.include_usage` 与 `usage.include` 已废弃且无效，usage 总是附带；usage 里还有 `cost`、`cost_details` 等扩展字段 | 文档 D2 |
| OpenRouter | 明确 `cached_tokens` 是缓存读、`cache_write_tokens` 是缓存写；写入只对有显式缓存与写入计费的模型返回 | 文档 D2 |

实测 `rider` 场景证明 async-openai 能同时读到同一块里的 `finish_reason` 与 usage（实测 P10），因此“DeepSeek 形态”不需要额外解析路径；真正要兼容的是“没有独立空 `choices` 块”，不能把空 `choices` 当成 usage 的唯一位置。

### 5.3 结构性兼容风险

- 必填字段缺失（`id`、`object`、`created`、`model`、`choices`）直接 `JSONDeserialize` 失败（源码 C2；实测 P6）。
- `usage` 的 `null` 与字段不存在无法区分（源码 C2）。
- `finish_reason` 可能缺失、可能为 `null`，也可能落在枚举外：`FinishReason` 只列 5 个值（[`chat_.rs` 第 1185–1191 行](https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L1185-L1191)），未列出的值会造成反序列化失败。

---

## 6. 中途错误、截断与超时

### 6.1 协议层允许的失败形态

OpenAI 规范在 `/chat/completions` 的 200 响应说明里写：

> If a failure occurs after streaming has started, a data frame may contain a JSON object with an `error` field instead of a completion chunk. The streaming schema describes successful JSON chunks, not error frames or the completion marker. An interrupted stream may end without the marker.

同处还规定正常结束的最后一帧是字面量 `data: [DONE]`，该帧没有 `event: done` 字段，并以空行结束（规范 O3 第 2565–2596 行）。API 参考页只重复了“流中断可能收不到 final usage chunk”（文档 O2），没有描述 error 帧。

### 6.2 async-openai 0.43 的实际表现

| 服务器行为 | 客户端观察 | 依据 |
|---|---|---|
| 正常：内容 → `finish_reason` → 空 `choices` 的 usage → `[DONE]` | 4 个 item，最后一块 usage 有值，流正常结束 | 实测 P1 |
| 不返回 usage | 所有 item 的 usage 为 `None`，流正常结束，无报错 | 实测 P2 |
| 有 `finish_reason` 但无 `[DONE]` | 流正常结束（`None`），无报错 | 实测 P3 |
| 半个 SSE 帧后关闭 | 残帧被静默丢弃，流正常结束，无报错 | 实测 P4 |
| 两个完整内容块后关闭，无 `finish_reason`、无 usage、无 `[DONE]` | 流正常结束，无报错；截断与正常短回答在协议层不可区分 | 实测 P5 |
| error 帧 | 流 item 为 `Err(JSONDeserialize(...))`，错误字符串里带原始 JSON：`{"error":{"message":"boom","type":"server_error"}}` | 实测 P6；源码 C7 |

原因在源码里很直接：`stream()` 对每个 `data` 事件只做 `serde_json::from_str::<O>`，失败就交给 `map_deserialization_error`，后者把原始载荷塞进错误文本（源码 C7，第 146–151 行）；`stream_mapped_raw_events` 只识别 `[DONE]` 与 `keepalive`，没有 `error` 事件分支（源码 C6，第 826–876 行）。因此 error 帧不会变成 `OpenAIError::ApiError`，拿不到 `code` / `type` 的强类型字段，只能从错误字符串里再解析一次。

### 6.3 超时

- reqwest 的 `timeout` 是“total request timeout … from when the request starts connecting until the response body has finished. Also considered a total deadline.”；`read_timeout` 是“applies to each read operation, and resets after a successful read”，官方建议用它检测“size isn’t known beforehand”的连接停滞（文档 Q1）。
- 实测：`timeout = 500 ms` 打到“首块后停 2 s”的流，流在第 2 块处中断（实测 P7）。
- 实测：`read_timeout = 500 ms` 同样在第 2 块处中断（实测 P8）；但对“每 200 ms 一块、总长 1.6 s”的流，8 块全部收到并正常结束（实测 P9），说明 `read_timeout` 是逐次读取的静默超时，不是总时限。
- 两种超时在 async-openai 侧都表现为 `StreamError(EventStream("Transport error: error decoding response body"))`（实测 P7、P8）。`client.rs` 把字节流的任何错误先转成 `std::io::Error::other`，再由 eventsource 转成字符串（源码 C6），`reqwest::Error::is_timeout()` 的信息在这一层已经丢失。

**推导**：要区分“超时”“连接被重置”“SSE 解码失败”，只能在应用层记录每块到达时间并自设静默阈值；依赖 async-openai 的错误类型做不到。

---

## 7. 事实对指标的约束

这些是事实层面的边界，不指定实现方式：

1. 服务端只按请求返回一份 usage，没有跨请求聚合。整页的缓存输入、非缓存输入、输出 token 只能由 man-ai 把各请求的数值相加；某个请求缺 usage 时，整页合计就只能是“已知部分 + 未知”。
2. 流式期间 token 数在最终 usage 块到达前不可知。OpenAI 没有定义“一个 chunk 等于一个 token”，实时 TPS 若在流中就要有值，只能用块数、字符数或类似代理量；结束后的准确 TPS 只能用 `completion_tokens` 计算。
3. `cached_tokens`、`cache_write_tokens`、`completion_tokens` 缺一即为未知，不能用 `prompt_tokens` 反推缓存命中，也不能把缓存输入并入非缓存输入。
4. “整页流逝时间”是客户端墙钟，与 usage 无关；请求的 wall time、首块时间、末块时间都只能由客户端打点。
5. 没有 `finish_reason` 的流一定不完整（实测 P5）；有 `finish_reason` 只说明生成段正常结束，usage 仍可能因中断缺失。`[DONE]` 缺失不能作为截断证据（实测 P3），usage 缺失也不能（实测 P2）。
6. OpenAI 现在推荐新项目用 Responses API，但明确“Chat Completions remains supported”（文档 O6）。本项目面向任意兼容端点，Chat Completions 仍是唯一共同分母。

---

## 8. 实测原始输出

```text
[normal] chunk#1 choices=1 content="Hello" finish=None usage=null
[normal] chunk#2 choices=1 content=" world" finish=None usage=null
[normal] chunk#3 choices=1 content="" finish=Some("Stop") usage=null
[normal] chunk#4 usage prompt=120 completion=7 total=127 cached=Some(64)
[normal] stream ended after 1.013333ms: chunks=4 usage_chunks=1
[nousage] chunk#1 choices=1 content="Hello" finish=None usage=null
[nousage] chunk#2 choices=1 content="" finish=Some("Stop") usage=null
[nousage] stream ended after 339.959µs: chunks=2 usage_chunks=0
[nodone] chunk#1 choices=1 content="Hello" finish=None usage=null
[nodone] chunk#2 choices=1 content="" finish=Some("Stop") usage=null
[nodone] stream ended after 339.917µs: chunks=2 usage_chunks=0
[partial] chunk#1 choices=1 content="Hello" finish=None usage=null
[partial] stream ended after 310.542µs: chunks=1 usage_chunks=0
[cut] chunk#1 choices=1 content="Hello" finish=None usage=null
[cut] chunk#2 choices=1 content=" wor" finish=None usage=null
[cut] stream ended after 386.25µs: chunks=2 usage_chunks=0
[rider] chunk#1 choices=1 content="Hello" finish=None usage=null
[rider] chunk#2 usage prompt=120 completion=7 total=127 cached=Some(64)
[rider] stream ended after 415.416µs: chunks=2 usage_chunks=1
[errorframe] chunk#1 choices=1 content="Hello" finish=None usage=null
[errorframe] chunk#2 ERR after 359.791µs: JSONDeserialize(Error("missing field `id`", line: 1, column: 50), "{\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}")
[total_timeout_on_slow] chunk#1 choices=1 content="a" finish=None usage=null
[total_timeout_on_slow] chunk#2 ERR after 502.862125ms: StreamError(EventStream("Transport error: error decoding response body"))
[read_timeout_on_slow] chunk#1 choices=1 content="a" finish=None usage=null
[read_timeout_on_slow] chunk#2 ERR after 502.899583ms: StreamError(EventStream("Transport error: error decoding response body"))
[read_timeout_on_drip] chunk#1..#8 content="x" finish=None usage=null
[read_timeout_on_drip] stream ended after 1.617312917s: chunks=8 usage_chunks=0
```

（`finish=Some("Stop")` 来自 async-openai 的 `FinishReason` 枚举；`chunk#3 content=""` 是把缺失的 `delta.content` 用 `unwrap_or_default()` 打印成了空串。）

---

## 9. 未确认

- usage 块与 `finish_reason` 块的严格先后：官方只承诺 usage 块在 `[DONE]` 之前，没有写“一定紧跟 finish_reason 块”。
- OpenAI 各模型是否都返回 `prompt_tokens_details.cached_tokens`：schema 默认 0，cookbook 说所有请求都会显示该字段（不足 1024 token 时为 0），本次没有对真实模型逐一验证。
- Chat Completions 是否在 `cache_write_tokens` 非零时把它计进 `prompt_tokens`：按字段定义与 Responses 的算式推导，官方没有 Chat 专用算式。
- TCP RST、HTTP 内容长度不符等异常关闭下 eventsource-stream 是否仍静默结束：本次只测了服务器正常关闭。
- DeepSeek、OpenRouter 的行为来自各自文档，没有对真实端点抓包；其他兼容端点（vLLM、Ollama、各类代理）的 usage 行为未核。
- OpenAI 流被取消时是否偶尔仍发送 usage 块：文档只说“可能收不到”，未实测。

---

## 10. 来源

### 文档

- **O1** Chat Completions API 参考（`stream`、`stream_options`、`usage` 字段与流式示例）：https://developers.openai.com/api/reference/resources/chat
- **O2** Chat Completions streaming events（`chat.completion.chunk` schema）：https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
- **O3** OpenAI OpenAPI 规范，提交 `995168c133dfc4f430a66fce08bdc3f4cd58cb82`：路由与 `[DONE]`/error 帧说明 https://github.com/openai/openai-openapi/blob/995168c133dfc4f430a66fce08bdc3f4cd58cb82/openapi.yaml#L2565-L2596 ；`ChatCompletionStreamOptions` https://github.com/openai/openai-openapi/blob/995168c133dfc4f430a66fce08bdc3f4cd58cb82/openapi.yaml#L44748-L44780 ；`CompletionUsage` https://github.com/openai/openai-openapi/blob/995168c133dfc4f430a66fce08bdc3f4cd58cb82/openapi.yaml#L45226-L45320 ；流式块 schema https://github.com/openai/openai-openapi/blob/995168c133dfc4f430a66fce08bdc3f4cd58cb82/openapi.yaml#L46643-L46780
- **O4** Prompt caching 指南（缓存分段、`cached_tokens` 与 `cache_write_tokens` 不重叠、普通输入算式）：https://developers.openai.com/api/docs/guides/prompt-caching
- **O5** Streaming API responses 指南（HTTP 流式经 server-sent events 传输）：https://developers.openai.com/api/docs/guides/streaming-responses
- **O6** Migrate to the Responses API（“While Chat Completions remains supported, Responses is recommended for all new projects.”）：https://developers.openai.com/api/docs/guides/migrate-to-responses
- **O7** OpenAI Cookbook，Prompt Caching 101（`cached_tokens` 表示 prompt 中被命中的 token）：https://github.com/openai/openai-cookbook/blob/main/examples/Prompt_Caching101.ipynb
- **O8** OpenAI Cookbook，Prompt Caching 201：https://github.com/openai/openai-cookbook/blob/main/examples/Prompt_Caching_201.ipynb
- **O9** Agents API Observability and usage（“Missing usage does not mean zero usage.”、“Cached tokens are included in `input_tokens`”）：https://developers.openai.com/api/docs/guides/agents-api/observability
- **Q1** `reqwest::ClientBuilder` 的 `timeout` 与 `read_timeout`：https://docs.rs/reqwest/0.13.5/reqwest/struct.ClientBuilder.html#method.timeout

### 源码（async-openai 0.43.0，tag `async-openai-v0.43.0`）

- **C1** `Chat::create_stream`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/chat.rs#L75-L92
- **C2** `CreateChatCompletionStreamResponse`、`ChatChoiceStream`、`ChatCompletionStreamResponseDelta`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L1326-L1400
- **C3** `ChatCompletionStreamOptions`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L1155-L1183
- **C4** `CompletionUsage` 与 `ChatCompletionResponseStream`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L104-L118 、https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/chat/chat_.rs#L1281-L1283
- **C5** `PromptTokensDetails`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/types/shared/prompt_tokens_details.rs#L1-L30
- **C6** SSE 解析与 `[DONE]`/keepalive 处理：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/client.rs#L826-L876
- **C7** `StreamError` 与 `map_deserialization_error`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/error.rs#L70-L79 、https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/error.rs#L146-L151
- **C8** 重试判定 `should_retry`：https://github.com/64bit/async-openai/blob/async-openai-v0.43.0/async-openai/src/middleware/retry/mod.rs#L58-L67

### 兼容端点文档

- **D1** DeepSeek Chat Completion（`stream_options`、`prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`、usage 挂最后一块）：https://api-docs.deepseek.com/api/create-chat-completion
- **D2** OpenRouter Usage Accounting（`stream_options.include_usage` 无效、`cached_tokens`/`cache_write_tokens` 语义、扩展字段）：https://openrouter.ai/docs/use-cases/usage-accounting

### 实测

- **P1–P10** 本文 §1.2 场景表对应输出，见 §8。探针为一次性本地程序（axum 0.8.9 假 SSE 服务 + async-openai 0.43.0 客户端），未提交到仓库。
