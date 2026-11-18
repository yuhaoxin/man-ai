//! man-ai：以 `MANPAGER` 为接入点包装平台 `man` 的翻译器。
//!
//! 薄壳模式设置 `MANPAGER` 后 `exec man`；分页器模式读完 stdin，按块并发翻译，
//! 走 OpenAI 兼容后端，带预转换与内容寻址缓存，TTY 下由内置 Markdown 阅读器显示。

pub mod backend;
pub mod blocks;
pub mod cache;
pub mod metrics;
pub mod cli;
pub mod config;
pub mod language;
pub mod markdown;
pub mod output;
pub mod prompt;
pub mod reader;
pub mod shellmode;
pub mod signals;
pub mod translate;
