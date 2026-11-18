//! 页首行、尾行与输出通道（规格 §8.4、§5.3）。
//!
//! TTY 路径把内容消息发给阅读器，由阅读器渲染；非 TTY 路径按块序直写 stdout，
//! 只有纯 Markdown 源文。

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;

use crate::backend::Usage;
use crate::blocks::Block;
use crate::signals::{self, CancelWatch};
use crate::translate::Outcome;

/// 页首行（§8.4）；页首行后阅读器紧跟一个空行。
pub fn header_text(language: &str) -> String {
    format!("── man-ai: translation ({language}) ──")
}

/// 尾行三形态（§8.4）。没有尝试过块时没有尾行。
pub fn tail_text(language: &str, outcome: &Outcome) -> Option<String> {
    if outcome.total == 0 {
        return None;
    }
    if !outcome.interrupted && outcome.adopted == outcome.total {
        return Some(format!("── man-ai: translated ({language}) ──"));
    }
    let reason = if outcome.interrupted {
        crate::backend::Reason::Interrupted
    } else {
        outcome.reason?
    };
    if outcome.deterministic || outcome.interrupted {
        Some(format!(
            "── man-ai: translation failed ({}), remaining blocks left untranslated ──",
            reason.as_str()
        ))
    } else {
        Some(format!(
            "── man-ai: partially translated ({}/{} blocks, {}) ──",
            outcome.adopted,
            outcome.total,
            reason.as_str()
        ))
    }
}

/// 阅读器内容消息；`Block` 与 `Live` 的 `gap` 是块前空行数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Header(String),
    /// 完成块：按 `gap` 补空行后整块出现（§8.1）。
    Block { gap: usize, body: String },
    /// 直播块的当前累积文本；同一块多次发送时替换直播段并重解析（§8.1）。
    Live { gap: usize, text: String },
    /// 状态行注记（§8.5）：`retrying` 或 `fallback: <reason>`。
    Note(String),
    /// 被采用请求的 usage 与生成窗口（§8.3）；usage 缺失即 `None`。
    Metric { usage: Option<Usage>, window: Duration },
    /// 整页缓存命中（§8.3）：状态行只显示 elapsed 与 cache hit。
    CacheHit,
    Diagnostic(String),
    Tail(String),
    Done,
}
enum Sink {
    /// 非 TTY：按块序直写 stdout。
    Stdout,
    /// TTY：发给阅读器。
    Channel(mpsc::Sender<Message>),
}

pub struct Out {
    sink: Sink,
    cancel: CancelWatch,
    /// 页首行语言，写尾行用。
    language: String,
    /// 写端已坏（EPIPE）：停止写入与提交。
    broken: bool,
}

impl Out {
    /// 非 TTY 路径。
    pub fn stdout() -> Self {
        let (_dropped, cancel) = signals::cancel_pair();
        Self::with_sink(Sink::Stdout, cancel)
    }

    /// TTY 路径：内容消息经 `sender` 交给阅读器。
    pub fn channel(sender: mpsc::Sender<Message>, cancel: CancelWatch) -> Self {
        Self::with_sink(Sink::Channel(sender), cancel)
    }

    fn with_sink(sink: Sink, cancel: CancelWatch) -> Self {
        Self {
            sink,
            cancel,
            language: String::new(),
            broken: false,
        }
    }

    /// 翻译请求用这个句柄观察取消。
    pub fn cancel(&self) -> CancelWatch {
        self.cancel.clone()
    }

    /// 写端是否已经因 EPIPE 关闭；调用方据此停止发起新请求。
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// 页首行；非 TTY 不写。
    pub fn write_header(&mut self, language: &str) -> io::Result<()> {
        self.language = language.to_string();
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::Header(header_text(language)));
        }
        Ok(())
    }

    /// 按 `gap_before` 补空行后写一个块的内容；非 TTY 每块立即 flush。
    pub fn write_block(&mut self, block: &Block, body: &str) -> io::Result<()> {
        if matches!(self.sink, Sink::Channel(_)) {
            let message = Message::Block {
                gap: block.gap_before,
                body: body.to_string(),
            };
            self.send(message);
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        let written = (0..block.gap_before)
            .try_for_each(|_| stdout.write_all(b"\n"))
            .and_then(|()| stdout.write_all(body.as_bytes()))
            .and_then(|()| stdout.flush());
        if written.is_err() {
            self.broken = true;
        }
        Ok(())
    }

    /// 尾行；非 TTY 不写。
    pub fn write_tail(&mut self, outcome: &Outcome) -> io::Result<()> {
        if !matches!(self.sink, Sink::Channel(_)) {
            return Ok(());
        }
        let Some(text) = tail_text(&self.language, outcome) else {
            return Ok(());
        };
        self.send(Message::Tail(text));
        Ok(())
    }

    /// 失败诊断：每进程最多一条，调用方保证；非 TTY 直写 stderr，TTY 转发给
    /// 阅读器在恢复终端后写出。
    pub fn diagnostic(&mut self, line: &str) {
        if matches!(self.sink, Sink::Stdout) {
            eprintln!("man-ai: {line}");
        } else {
            self.send(Message::Diagnostic(line.to_string()));
        }
    }

    /// 收尾：通知阅读器全部内容已定；非 TTY 只 flush。两条路径退出码都是 0。
    pub fn finish(mut self) -> io::Result<i32> {
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::Done);
            return Ok(0);
        }
        io::stdout().lock().flush()?;
        Ok(0)
    }

    /// 直播块的当前累积文本；非 TTY 不写（增量只进阅读器，§5.3）。
    pub fn write_live(&mut self, gap: usize, text: &str) -> io::Result<()> {
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::Live {
                gap,
                text: text.to_string(),
            });
        }
        Ok(())
    }

    /// 状态行注记；非 TTY 不写。
    pub fn note(&mut self, text: &str) {
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::Note(text.to_string()));
        }
    }

    /// 被采用请求的 usage 与生成窗口（§8.3）；非 TTY 不写。
    pub fn metric(&mut self, usage: Option<Usage>, window: Duration) {
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::Metric { usage, window });
        }
    }

    /// 整页缓存命中（§8.3）；非 TTY 不写。
    pub fn cache_hit(&mut self) {
        if matches!(self.sink, Sink::Channel(_)) {
            self.send(Message::CacheHit);
        }
    }

    /// 发送并跟踪写端：阅读器退出后发送失败即视为 EPIPE。
    fn send(&mut self, message: Message) {
        let failed = match &self.sink {
            Sink::Stdout => return,
            Sink::Channel(sender) => sender.send(message).is_err(),
        };
        if failed {
            self.broken = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Reason;

    fn outcome(adopted: usize, total: usize, reason: Option<Reason>) -> Outcome {
        Outcome {
            adopted,
            total,
            reason,
            deterministic: false,
            interrupted: false,
        }
    }

    #[test]
    fn 页首行按规格_8_4() {
        assert_eq!(header_text("zh-CN"), "── man-ai: translation (zh-CN) ──");
    }

    #[test]
    fn 尾行三形态与零块() {
        assert_eq!(
            tail_text("zh-CN", &outcome(2, 2, None)).unwrap(),
            "── man-ai: translated (zh-CN) ──"
        );
        assert_eq!(
            tail_text("zh-CN", &outcome(1, 2, Some(Reason::BackendError))).unwrap(),
            "── man-ai: partially translated (1/2 blocks, backend error) ──"
        );
        let mut deterministic = outcome(0, 2, Some(Reason::NotConfigured));
        deterministic.deterministic = true;
        assert_eq!(
            tail_text("zh-CN", &deterministic).unwrap(),
            "── man-ai: translation failed (not configured), remaining blocks left untranslated ──"
        );
        let mut interrupted = outcome(1, 2, None);
        interrupted.interrupted = true;
        interrupted.deterministic = true;
        assert_eq!(
            tail_text("zh-CN", &interrupted).unwrap(),
            "── man-ai: translation failed (interrupted), remaining blocks left untranslated ──"
        );
    }

    #[test]
    fn 通道路径把首块尾_done_按序转发() {
        let (sender, receiver) = mpsc::channel();
        let (trigger, cancel) = crate::signals::cancel_pair();
        drop(trigger);
        let mut out = Out::channel(sender, cancel);
        out.write_header("zh-CN").unwrap();
        let block = Block {
            gap_before: 0,
            raw: b"x\n".to_vec(),
            pre: "x\n".to_string(),
        };
        out.write_block(&block, "译\n").unwrap();
        out.write_tail(&outcome(1, 1, None)).unwrap();
        out.finish().unwrap();

        let received: Vec<Message> = receiver.try_iter().collect();
        assert_eq!(
            received,
            vec![
                Message::Header("── man-ai: translation (zh-CN) ──".to_string()),
                Message::Block {
                    gap: 0,
                    body: "译\n".to_string()
                },
                Message::Tail("── man-ai: translated (zh-CN) ──".to_string()),
                Message::Done,
            ]
        );
    }

    #[test]
    fn 阅读器退出后发送失败置_broken() {
        let (sender, receiver) = mpsc::channel();
        let (_trigger, cancel) = crate::signals::cancel_pair();
        drop(receiver);
        let mut out = Out::channel(sender, cancel);
        let block = Block {
            gap_before: 0,
            raw: b"x\n".to_vec(),
            pre: "x\n".to_string(),
        };
        out.write_block(&block, "译\n").unwrap();
        assert!(out.broken());
        // 写端坏掉后尾行不再发。
        out.write_tail(&outcome(1, 1, None)).unwrap();
        let _ = out.finish();
    }

    #[test]
    fn 通道路径转发指标事件与缓存命中() {
        let (sender, receiver) = mpsc::channel();
        let (_trigger, cancel) = crate::signals::cancel_pair();
        let mut out = Out::channel(sender, cancel);
        out.metric(None, Duration::from_secs(1));
        out.cache_hit();
        assert_eq!(
            receiver.try_iter().collect::<Vec<Message>>(),
            vec![
                Message::Metric {
                    usage: None,
                    window: Duration::from_secs(1)
                },
                Message::CacheHit,
            ]
        );
    }
}
