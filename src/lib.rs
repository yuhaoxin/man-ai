//! man-ai：以 `MANPAGER` 为接入点包装平台 `man` 的翻译器。
//!
//! 本票（#18）只实现透传骨架：自有参数、薄壳/分页器双模式、内层分页器链。
//! 不调用 AI，页面始终原文。

pub mod cli;
pub mod language;
pub mod pager;
pub mod shellmode;
