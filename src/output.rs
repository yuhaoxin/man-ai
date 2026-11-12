//! 页首行、尾行与写端（规格 §8.3、§5.2、§5.3）。
//!
//! TTY 路径先把整页收进内存，全部写完后才启动内层分页器（首屏流式是后续票）；
//! 非 TTY 路径直写 stdout、每个块立即 flush，没有页首行与尾行。

use std::ffi::OsString;
use std::io::{self, Write};

use crate::blocks::Block;
use crate::pager;
use crate::translate::Outcome;

/// 页首行后紧跟一个空行。
pub fn header_line(language: &str) -> String {
    format!("── man-ai: translation ({language}) ──\n\n")
}

/// 尾行三形态（§8.3）。`outcome.total == 0` 时没有尾行。
pub fn tail_line(language: &str, outcome: &Outcome) -> Option<String> {
    if outcome.total == 0 {
        return None;
    }
    if outcome.adopted == outcome.total {
        return Some(format!("── man-ai: translated ({language}) ──\n"));
    }
    let reason = outcome.reason?;
    if outcome.deterministic {
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

pub struct Out {
    tty: bool,
    buffer: Vec<u8>,
}

impl Out {
    pub fn new(tty: bool) -> Self {
        Self {
            tty,
            buffer: Vec::new(),
        }
    }

    pub fn is_tty(&self) -> bool {
        self.tty
    }

    /// 页首行；非 TTY 不写。
    pub fn write_header(&mut self, language: &str) -> io::Result<()> {
        if self.tty {
            self.buffer
                .extend_from_slice(header_line(language).as_bytes());
        }
        Ok(())
    }

    /// 按 `gap_before` 补空行后写一个块的内容。
    pub fn write_block(&mut self, block: &Block, body: &[u8]) -> io::Result<()> {
        if self.tty {
            self.buffer
                .extend(std::iter::repeat_n(b'\n', block.gap_before));
            self.buffer.extend_from_slice(body);
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        for _ in 0..block.gap_before {
            stdout.write_all(b"\n")?;
        }
        stdout.write_all(body)?;
        stdout.flush()
    }

    /// 尾行；非 TTY 或没有尝试过块时不写。
    pub fn write_tail(&mut self, language: &str, outcome: &Outcome) -> io::Result<()> {
        if !self.tty {
            return Ok(());
        }
        let Some(line) = tail_line(language, outcome) else {
            return Ok(());
        };
        if !self.buffer.is_empty() && !self.buffer.ends_with(b"\n") {
            self.buffer.push(b'\n');
        }
        self.buffer.push(b'\n');
        self.buffer.extend_from_slice(line.as_bytes());
        Ok(())
    }

    /// 收尾：TTY 交给内层分页器链（不可用则直写 stdout），非 TTY 只 flush。
    pub fn finish(self, rest: &[OsString]) -> io::Result<i32> {
        if !self.tty {
            io::stdout().lock().flush()?;
            return Ok(0);
        }
        if let Some(chain) = pager::resolve() {
            if let Ok(code) = pager::pipe(&chain, rest, &self.buffer) {
                return Ok(code);
            }
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(&self.buffer)?;
        stdout.flush()?;
        Ok(0)
    }
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
        }
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
    fn 非_tty_不写页首尾行() {
        let mut out = Out::new(false);
        out.write_header("Japanese").unwrap();
        let block = split(b"a\n").blocks.remove(0);
        out.write_block(&block, b"a\n").unwrap();
        out.write_tail("Japanese", &outcome(1, 1, None, false))
            .unwrap();
        assert!(out.buffer.is_empty());
    }

    #[test]
    fn tty_按序拼页且空行数还原() {
        let mut out = Out::new(true);
        out.write_header("Japanese").unwrap();
        for (index, block) in split(b"a\n\n\nb").blocks.iter().enumerate() {
            out.write_block(block, if index == 0 { b"A\n" } else { b"B" })
                .unwrap();
        }
        out.write_tail("Japanese", &outcome(2, 2, None, false))
            .unwrap();
        assert_eq!(
            String::from_utf8(out.buffer).unwrap(),
            "── man-ai: translation (Japanese) ──\n\nA\n\n\nB\n\n── man-ai: translated (Japanese) ──\n"
        );
    }
}
