//! 分页文本的切块与逐字节重组（规格 §6）。
//!
//! 切块只用于定位空行，不改动任何字节：`raw` 是块内原始字节，`text` 是它的
//! UTF-8 解码结果，解码失败时为 `None`（该块不送模型）。

/// 一个翻译单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 前置空行数。
    pub gap_before: usize,
    /// 原始字节，回退时原样输出。
    pub raw: Vec<u8>,
    /// `raw` 的 UTF-8 解码结果；非 UTF-8 时为 `None`。
    pub text: Option<String>,
}

/// 整页切块结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPage {
    pub blocks: Vec<Block>,
    /// 最后一个块之后的空行数。
    pub tail_gap: usize,
    /// 输入为空或只有空行。
    pub empty_input: bool,
}

/// 按 `0x0A` 切行，把连续非空行聚成块（§6.1）。
///
/// 空行是零字节行：只含空格或制表符的行算非空，连同空白进入块。
/// `gap_before` 与 `tail_gap` 记录空行数，重组时必须逐字节还原输入。
pub fn split(input: &[u8]) -> SplitPage {
    let mut blocks = Vec::new();
    let mut pending_gap = 0;
    let mut current: Option<(usize, Vec<u8>)> = None;
    let mut pos = 0;

    while pos < input.len() {
        let (end, content_end) = match input[pos..].iter().position(|&b| b == b'\n') {
            Some(offset) => (pos + offset + 1, pos + offset),
            None => (input.len(), input.len()),
        };
        if content_end == pos && end > pos {
            // 零字节行：先记进待定空行数，等下一个非空行或页尾决定归属。
            pending_gap += 1;
        } else {
            match &mut current {
                Some((_, raw)) if pending_gap == 0 => raw.extend_from_slice(&input[pos..end]),
                _ => {
                    if let Some((gap, raw)) = current.take() {
                        blocks.push(Block {
                            gap_before: gap,
                            text: std::str::from_utf8(&raw).ok().map(str::to_owned),
                            raw,
                        });
                    }
                    current = Some((pending_gap, input[pos..end].to_vec()));
                    pending_gap = 0;
                }
            }
        }
        pos = end;
    }

    if let Some((gap, raw)) = current {
        blocks.push(Block {
            gap_before: gap,
            text: std::str::from_utf8(&raw).ok().map(str::to_owned),
            raw,
        });
    }

    SplitPage {
        empty_input: blocks.is_empty(),
        blocks,
        tail_gap: pending_gap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试侧重组：按 `gap_before` + raw + `tail_gap` 还原字节流。
    fn join(page: &SplitPage) -> Vec<u8> {
        let mut out = Vec::new();
        for block in &page.blocks {
            out.extend(std::iter::repeat_n(b'\n', block.gap_before));
            out.extend_from_slice(&block.raw);
        }
        out.extend(std::iter::repeat_n(b'\n', page.tail_gap));
        out
    }

    #[test]
    fn 空输入与只有空行都是零块() {
        for input in [&b""[..], b"\n", b"\n\n\n"] {
            let page = split(input);
            assert!(page.empty_input, "{input:?}");
            assert!(page.blocks.is_empty(), "{input:?}");
            assert_eq!(join(&page), input, "{input:?}");
        }
        assert_eq!(split(b"").tail_gap, 0);
        assert_eq!(split(b"\n\n\n").tail_gap, 3);
    }

    #[test]
    fn 连续非空行聚成一块() {
        let page = split(b"NAME\n     ls - list\n");
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].gap_before, 0);
        assert_eq!(page.blocks[0].raw, b"NAME\n     ls - list\n");
        assert_eq!(
            page.blocks[0].text.as_deref(),
            Some("NAME\n     ls - list\n")
        );
        assert_eq!(page.tail_gap, 0);
    }

    #[test]
    fn 空行数量记进_gap_before_与_tail_gap() {
        let page = split(b"a\n\n\nb\n\n");
        assert_eq!(page.blocks.len(), 2);
        assert_eq!(page.blocks[1].gap_before, 2);
        assert_eq!(page.tail_gap, 1);
        assert_eq!(join(&page), b"a\n\n\nb\n\n");
    }

    #[test]
    fn 只含空格的行算非空() {
        let page = split(b"a\n   \nb\n");
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].raw, b"a\n   \nb\n");
    }

    #[test]
    fn 末行无换行符时原样还原() {
        let input = b"HEADER\n\nbody without newline";
        let page = split(input);
        assert_eq!(page.blocks.len(), 2);
        assert_eq!(page.blocks[1].raw, b"body without newline");
        assert_eq!(join(&page), input);
    }

    #[test]
    fn 切分再合并在多种输入下逐字节一致() {
        let inputs: &[&[u8]] = &[
            b"",
            b"\n",
            b"a",
            b"a\n",
            b"a\n\n",
            b"\n\na",
            b"a\n\n\nb",
            b"a\n   \nb\n\n\t\nc\n",
            b"page header\n\nbody one\n  continued\n\nbody two\n\nfooter\n",
            b"\n\n\nmulti\n\n\nblank\n\n\n",
            b"\xff\xfe binary\n\nok\n",
            b"line with trailing spaces   \n\n\x1b[1mbold\x1b[0m\n",
        ];
        for input in inputs {
            let page = split(input);
            assert_eq!(join(&page), *input, "{input:?}");
        }
    }

    #[test]
    fn 非_utf8_块的_text_为_none() {
        let page = split(b"ok\n\n\xff\xfe\n");
        assert_eq!(page.blocks.len(), 2);
        assert!(page.blocks[0].text.is_some());
        assert!(page.blocks[1].text.is_none());
        assert_eq!(page.blocks[1].raw, b"\xff\xfe\n");
    }

    #[test]
    fn 空行只统计零字节行() {
        let page = split(b"\n \n\t\n\n");
        // 中间两行只含空白，算同一块。
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].gap_before, 1);
        assert_eq!(page.blocks[0].raw, b" \n\t\n");
        assert_eq!(page.tail_gap, 1);
    }
}
