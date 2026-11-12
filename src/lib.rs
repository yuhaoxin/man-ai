//! man-ai：以 `MANPAGER` 为接入点包装平台 `man` 的翻译器。
//!
//! 薄壳模式设置 `MANPAGER` 后 `exec man`；分页器模式按块并发翻译 stdin 上的
//! 渲染页面，走 OpenAI 兼容后端，带结构校验、内容寻址缓存与 `-k`/`-f` 描述批翻译。

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
