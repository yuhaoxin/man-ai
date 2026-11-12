//! 页首行、尾行与流式写端（规格 §8.1–§8.4、§5.6）。
//!
//! TTY 路径先把输出留在内存里；已写行数达到终端高度（或全部块处理完）才启动
//! 内层分页器，此后每写一块立即 flush。非 TTY 路径按块序直写 stdout。

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::{Child, ChildStdin};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::blocks::Block;
use crate::pager;
use crate::signals::Interrupt;
use crate::translate::Outcome;

/// 取不到终端高度时的行数（§8.1）。
const FALLBACK_HEIGHT: usize = 24;

/// 页首行后紧跟一个空行。
pub fn header_line(language: &str) -> String {
    format!("── man-ai: translation ({language}) ──\n\n")
}

/// 尾行三形态（§8.3）。`outcome.total == 0` 时没有尾行。
pub fn tail_line(language: &str, outcome: &Outcome) -> Option<String> {
    if outcome.total == 0 {
        return None;
    }
    if !outcome.interrupted && outcome.adopted == outcome.total {
        return Some(format!("── man-ai: translated ({language}) ──\n"));
    }
    let reason = if outcome.interrupted {
        crate::backend::Reason::Interrupted
    } else {
        outcome.reason?
    };
    if outcome.deterministic || outcome.interrupted {
        Some(format!(
            "── man-ai: translation failed ({}), remaining blocks left untranslated ──\n",
            reason.as_str()
        ))
    } else {
        Some(format!(
            "── man-ai: partially translated ({}/{} blocks, {}) ──\n",
            outcome.adopted,
            outcome.total,
            reason.as_str()
        ))
    }
}

/// 内层分页器：持有子进程与 stdin 写端，全部写完后才关写端并 `wait`。
struct Pager {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Pager {
    /// 写失败即 EPIPE：丢弃写端，之后的写入不再尝试。
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        if let Err(error) = stdin.write_all(bytes).and_then(|()| stdin.flush()) {
            self.stdin = None;
            return Err(error);
        }
        Ok(())
    }

    fn wait(mut self) -> io::Result<i32> {
        drop(self.stdin.take());
        self.child.wait().map(pager::exit_code)
    }
}

pub struct Out {
    tty: bool,
    /// 原样追加到分页器命令行的剩余 argv。
    rest: Vec<OsString>,
    language: String,
    height: usize,
    /// 分页器启动前的输出。
    buffered: Vec<u8>,
    pager: Option<Pager>,
    /// 已经尝试过启动分页器（成功、不可用或启动失败都算，不重试）。
    launched: bool,
    /// 写端已坏（EPIPE），停止写入。
    broken: bool,
    progress: bool,
    /// 已写出的换行数，与末尾是否有未换行的半行（末行无换行算一行）。
    newlines: usize,
    partial: bool,
    interrupt: Option<Interrupt>,
}

impl Out {
    pub fn new(tty: bool, rest: Vec<OsString>) -> Self {
        Self {
            tty,
            rest,
            language: String::new(),
            height: terminal_height(),
            buffered: Vec::new(),
            pager: None,
            launched: false,
            broken: false,
            progress: false,
            newlines: 0,
            partial: false,
            interrupt: tty.then(Interrupt::install),
        }
    }

    /// 写端是否已经因 EPIPE 关闭；调用方据此停止发起新请求。
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// 第一次 Ctrl-C 的标志；非 TTY 路径没有处理器。
    pub fn interrupt_flag(&self) -> Option<Arc<AtomicBool>> {
        self.interrupt.as_ref().map(Interrupt::flag)
    }

    /// 页首行；非 TTY 不写。
    pub fn write_header(&mut self, language: &str) -> io::Result<()> {
        self.language = language.to_string();
        if self.tty {
            self.emit(header_line(language).as_bytes());
        }
        Ok(())
    }

    /// 按 `gap_before` 补空行后写一个块的内容；非 TTY 每块立即 flush。
    pub fn write_block(&mut self, block: &Block, body: &[u8]) -> io::Result<()> {
        if !self.tty {
            let mut stdout = io::stdout().lock();
            let written = (0..block.gap_before)
                .try_for_each(|_| stdout.write_all(b"\n"))
                .and_then(|()| stdout.write_all(body))
                .and_then(|()| stdout.flush());
            if written.is_err() {
                self.broken = true;
            }
            return Ok(());
        }
        for _ in 0..block.gap_before {
            self.emit(b"\n");
        }
        self.emit(body);
        // §8.4：首个块写出且分页器尚未启动时提示一次。
        if !self.progress && self.pager.is_none() && !self.broken {
            self.progress = true;
            eprintln!("man-ai: translating ({})…", self.language);
        }
        Ok(())
    }

    /// 尾行；非 TTY 或没有尝试过块时不写。
    pub fn write_tail(&mut self, outcome: &Outcome) -> io::Result<()> {
        if !self.tty || self.broken {
            return Ok(());
        }
        let Some(line) = tail_line(&self.language, outcome) else {
            return Ok(());
        };
        if self.wrote() && !self.at_line_start() {
            self.emit(b"\n");
        }
        self.emit(b"\n");
        self.emit(line.as_bytes());
        Ok(())
    }

    /// 分页器启动后 man-ai 不写 stderr（§5.5）；此前每行诊断一次。
    pub fn diagnostic(&self, line: &str) {
        if self.pager.is_none() {
            eprintln!("man-ai: {line}");
        }
    }

    /// 收尾：关闭分页器写端并透传其退出码；链不可用时直写 stdout 退出 0。
    pub fn finish(mut self) -> io::Result<i32> {
        if !self.tty {
            io::stdout().lock().flush()?;
            return Ok(0);
        }
        // 全部块处理完毕仍未达首屏阈值：现在启动。
        self.launch();
        match self.pager.take() {
            Some(pager) => pager.wait(),
            None => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(&self.buffered)?;
                stdout.flush()?;
                Ok(0)
            }
        }
    }

    /// 写一段字节：分页器已启动就直接写，否则留在内存并在达到首屏阈值时启动。
    fn emit(&mut self, bytes: &[u8]) {
        self.count(bytes);
        if let Some(pager) = self.pager.as_mut() {
            if pager.send(bytes).is_err() {
                self.broken = true;
            }
            return;
        }
        self.buffered.extend_from_slice(bytes);
        if self.lines() >= self.height {
            self.launch();
        }
    }

    fn count(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.newlines += bytes.iter().filter(|byte| **byte == b'\n').count();
        self.partial = !bytes.ends_with(b"\n");
    }

    fn lines(&self) -> usize {
        self.newlines + usize::from(self.partial)
    }

    fn wrote(&self) -> bool {
        self.newlines > 0 || self.partial
    }

    fn at_line_start(&self) -> bool {
        !self.partial
    }

    /// 启动内层分页器：先恢复 SIGINT 默认处置，再把内存里的输出一次性写进去。
    fn launch(&mut self) {
        if self.launched || !self.tty {
            return;
        }
        self.launched = true;
        if let Some(mut interrupt) = self.interrupt.take() {
            interrupt.restore();
        }
        let Some(chain) = pager::resolve() else {
            return;
        };
        let Ok((child, stdin)) = pager::start(&chain, &self.rest) else {
            return;
        };
        let mut pager = Pager {
            child,
            stdin: Some(stdin),
        };
        if pager.send(&self.buffered).is_err() {
            self.broken = true;
        }
        self.buffered.clear();
        self.pager = Some(pager);
    }
}

/// 终端高度：`/dev/tty` 的 `TIOCGWINSZ`，取不到按 24（§8.1）。
fn terminal_height() -> usize {
    std::fs::File::open("/dev/tty")
        .ok()
        .and_then(terminal_size::terminal_size_of)
        .map_or(FALLBACK_HEIGHT, |(_, height)| usize::from(height.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Reason;
    use crate::blocks::split;

    fn outcome(
        adopted: usize,
        total: usize,
        reason: Option<Reason>,
        deterministic: bool,
    ) -> Outcome {
        Outcome {
            adopted,
            total,
            reason,
            deterministic,
            interrupted: false,
        }
    }

    /// 固定高度、不启动分页器，只观察内存里的字节与行数计数。
    fn buffered_out() -> Out {
        let mut out = Out::new(true, Vec::new());
        out.interrupt = None;
        out.height = usize::MAX;
        out
    }

    #[test]
    fn 页首行文案() {
        assert_eq!(
            header_line("Simplified Chinese"),
            "── man-ai: translation (Simplified Chinese) ──\n\n"
        );
    }

    #[test]
    fn 尾行三形态() {
        assert_eq!(
            tail_line("Japanese", &outcome(3, 3, None, false)).unwrap(),
            "── man-ai: translated (Japanese) ──\n"
        );
        assert_eq!(
            tail_line(
                "Japanese",
                &outcome(2, 3, Some(Reason::BackendError), false)
            )
            .unwrap(),
            "── man-ai: partially translated (2/3 blocks, backend error) ──\n"
        );
        assert_eq!(
            tail_line(
                "Japanese",
                &outcome(0, 3, Some(Reason::NotConfigured), true)
            )
            .unwrap(),
            "── man-ai: translation failed (not configured), remaining blocks left untranslated ──\n"
        );
        assert_eq!(tail_line("Japanese", &outcome(0, 0, None, false)), None);
    }

    #[test]
    fn 中断尾行报_interrupted() {
        let mut interrupted = outcome(1, 3, Some(Reason::Timeout), true);
        interrupted.interrupted = true;
        assert_eq!(
            tail_line("Japanese", &interrupted).unwrap(),
            "── man-ai: translation failed (interrupted), remaining blocks left untranslated ──\n"
        );
    }

    #[test]
    fn 行数含页首行与半行() {
        let mut out = buffered_out();
        out.write_header("Japanese").unwrap();
        assert_eq!(out.lines(), 2);
        let block = split(b"a\nb").blocks.remove(0);
        out.write_block(&block, b"A\nB").unwrap();
        assert_eq!(out.lines(), 4);
        assert!(!out.at_line_start());
    }

    #[test]
    fn 行数达到终端高度即首屏就绪() {
        let mut out = buffered_out();
        out.height = 4;
        let blocks = split(b"a\n\nb\n").blocks;
        out.write_header("Japanese").unwrap();
        out.write_block(&blocks[0], b"A\n").unwrap();
        assert!(out.lines() < out.height);
        out.write_block(&blocks[1], b"B\n").unwrap();
        assert!(out.lines() >= out.height, "第 4 行达到高度 4");
    }

    #[test]
    fn 非_tty_不写页首尾行() {
        let mut out = Out::new(false, Vec::new());
        out.write_header("Japanese").unwrap();
        let block = split(b"a\n").blocks.remove(0);
        out.write_block(&block, b"a\n").unwrap();
        out.write_tail(&outcome(1, 1, None, false)).unwrap();
        assert!(out.buffered.is_empty());
    }

    #[test]
    fn tty_按序拼页且空行数还原() {
        let mut out = buffered_out();
        out.write_header("Japanese").unwrap();
        for (index, block) in split(b"a\n\n\nb").blocks.iter().enumerate() {
            out.write_block(block, if index == 0 { b"A\n" } else { b"B" })
                .unwrap();
        }
        out.write_tail(&outcome(2, 2, None, false)).unwrap();
        assert_eq!(
            String::from_utf8(out.buffered).unwrap(),
            "── man-ai: translation (Japanese) ──\n\nA\n\n\nB\n\n── man-ai: translated (Japanese) ──\n"
        );
    }
}
