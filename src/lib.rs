//! man-ai：以 `MANPAGER` 为接入点包装平台 `man` 的翻译器。
//!
//! #18 实现透传骨架：自有参数、薄壳/分页器双模式、内层分页器链。
//! #19 加入语言来源链、显示名与配置文件解析。
//! #20 加入切块、提示词、OpenAI 兼容后端、结构校验与串行翻译管线。

pub mod backend;
pub mod blocks;
pub mod cli;
pub mod config;
pub mod language;
pub mod output;
pub mod pager;
pub mod prompt;
pub mod shellmode;
pub mod translate;
pub mod validate;
