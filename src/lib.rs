//! man-ai：以 `MANPAGER` 为接入点包装平台 `man` 的翻译器。
//!
//! #18 实现透传骨架：自有参数、薄壳/分页器双模式、内层分页器链。
//! #19 加入语言来源链、显示名与配置文件解析。
//! #20 加入切块、提示词、OpenAI 兼容后端、结构校验与串行翻译管线。
//! #21 加入按序并发、首屏流式、EPIPE 提前退出与两阶段 SIGINT。
//! #22 加入块级内容寻址缓存。
//! #23 加入 `-k`/`-f` 拦截与描述批翻译。

pub mod backend;
pub mod blocks;
pub mod cache;
pub mod cli;
pub mod config;
pub mod klist;
pub mod language;
pub mod output;
pub mod pager;
pub mod prompt;
pub mod shellmode;
pub mod signals;
pub mod translate;
pub mod validate;
