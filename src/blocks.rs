//! 分页文本的切块、预转换与逐字节重组（规格 §6）。
//!
//! 切块只用于定位空行，不改动任何字节：`raw` 是块内原始字节，`pre` 是它的
//! 预转换结果（Markdown 文本），既是送模型的唯一内容，也是回退输出。

/// 一个翻译单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 前置空行数。
    pub gap_before: usize,
    /// 原始渲染字节。
    pub raw: Vec<u8>,
    /// 预转换结果（§6.3）；非 UTF-8 字节按 U+FFFD 有损解码。
    pub pre: String,
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
                            pre: preconvert(&raw),
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
            pre: preconvert(&raw),
            raw,
        });
    }

    SplitPage {
        empty_input: blocks.is_empty(),
        blocks,
        tail_gap: pending_gap,
    }
}

/// 样式位：粗体与下划线。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    underline: bool,
}

impl Style {
    fn union(self, other: Style) -> Style {
        Style {
            bold: self.bold || other.bold,
            underline: self.underline || other.underline,
        }
    }
}

/// 渲染字节到 Markdown 的确定性预转换（§6.3）。
///
/// SGR 粗体与退格叠加 `X\x08X` 转成 `**…**`，SGR 下划线与 `X\x08_` 转成
/// `*…*`；其余控制序列剥除；文本逐字节保留；非 UTF-8 字节按 U+FFFD 有损
/// 解码。块尾未关闭的样式补上闭标记，保证产物是合法 Markdown。
pub fn preconvert(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    // `open` 是已写出标记的样式位；SGR 与退格叠加各自给出目标样式，靠标记转移合并相邻同样式。
    let mut sgr = Style::default();
    let mut open = Style::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\x1b' {
            i = escape(&chars, i, &mut sgr);
            set(&mut out, &mut open, sgr);
            continue;
        }
        if c == '\x08' || (c.is_control() && c != '\n' && c != '\t') {
            set(&mut out, &mut open, sgr);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'\x08')
            && let Some((shown, style)) = overstrike(&chars, i, &mut i)
        {
            set(&mut out, &mut open, sgr.union(style));
            out.push(shown);
            continue;
        }
        set(&mut out, &mut open, sgr);
        out.push(c);
        i += 1;
    }
    // 块尾补闭标记；闭标记放在末行换行符之前，避免悬垂到下一行。
    let mut close = String::new();
    set(&mut close, &mut open, Style::default());
    if let Some(body) = out.strip_suffix('\n') {
        out.truncate(body.len());
        out.push_str(&close);
        out.push('\n');
    } else {
        out.push_str(&close);
    }
    out
}

/// 从 `i` 起识别一次退格叠加，返回显示字符与样式；不匹配时 `i` 不动。
///
/// `X\x08X` 是粗体，`X\x08_` 与 `_\x08X` 是下划线；同字符的三重叠加
/// （`X\x08X\x08X`）只显示一次。
fn overstrike(chars: &[char], i: usize, next: &mut usize) -> Option<(char, Style)> {
    let c = chars[i];
    let d = *chars.get(i + 2)?;
    let mut style = Style::default();
    if d == c {
        style.bold = true;
    } else if d == '_' || c == '_' {
        style.underline = true;
    } else {
        return None;
    }
    let shown = if c == '_' && d != c { d } else { c };
    *next = i + 3;
    while chars.get(*next) == Some(&'\x08') && chars.get(*next + 1) == Some(&shown) {
        *next += 2;
    }
    Some((shown, style))
}

/// 从 `i` 起消费一个 ESC 序列，返回下一个位置；CSI 以 `m` 结束时更新 SGR 样式位。
///
/// 非 CSI 序列（中间字节 0x20–0x2F 加一个终结字节）与非 `m` 终结的 CSI 只
/// 剥除。序列被输入截断时消费到末尾。
fn escape(chars: &[char], i: usize, sgr: &mut Style) -> usize {
    if chars.get(i + 1) != Some(&'[') {
        let mut j = i + 1;
        while matches!(chars.get(j), Some(c) if ('\u{20}'..='\u{2f}').contains(c)) {
            j += 1;
        }
        return (j + 1).min(chars.len());
    }
    let start = i + 2;
    let mut j = start;
    while matches!(chars.get(j), Some(c) if ('\u{20}'..='\u{3f}').contains(c)) {
        j += 1;
    }
    match chars.get(j) {
        Some('m') => {
            apply_sgr(&chars[start..j], sgr);
            j + 1
        }
        Some(_) => j + 1,
        None => chars.len(),
    }
}

/// SGR 参数只认识 1/4（开）与 0/22/24（关）；其余参数剥除、不改样式。
fn apply_sgr(params: &[char], sgr: &mut Style) {
    let params: String = params.iter().collect();
    for part in params.split(';') {
        match part {
            "" | "0" => *sgr = Style::default(),
            "1" => sgr.bold = true,
            "4" => sgr.underline = true,
            "22" => sgr.bold = false,
            "24" => sgr.underline = false,
            _ => {}
        }
    }
}

/// 按目标样式补开/闭标记：先闭下划线再闭粗体，先开粗体再开下划线。
fn set(out: &mut String, open: &mut Style, target: Style) {
    if open.underline && !target.underline {
        out.push('*');
    }
    if open.bold && !target.bold {
        out.push_str("**");
    }
    if !open.bold && target.bold {
        out.push_str("**");
    }
    if !open.underline && target.underline {
        out.push('*');
    }
    *open = target;
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
        assert_eq!(page.blocks[0].pre, "NAME\n     ls - list\n");
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
    fn 非_utf8_块有损解码进_pre() {
        let page = split(b"ok\n\n\xff\xfe\n");
        assert_eq!(page.blocks.len(), 2);
        assert_eq!(page.blocks[0].pre, "ok\n");
        assert_eq!(page.blocks[1].raw, b"\xff\xfe\n");
        assert_eq!(page.blocks[1].pre, "\u{fffd}\u{fffd}\n");
    }

    #[test]
    fn 预转换_sgr_粗体与下划线() {
        assert_eq!(
            preconvert(b"\x1b[1mbold\x1b[0m plain\n"),
            "**bold** plain\n"
        );
        assert_eq!(
            preconvert(b"\x1b[4munder\x1b[0m plain\n"),
            "*under* plain\n"
        );
        // 22/24 是粗体/下划线的单独关闭。
        assert_eq!(preconvert(b"\x1b[1mb\x1b[22mc\n"), "**b**c\n");
        assert_eq!(preconvert(b"\x1b[4mu\x1b[24mv\n"), "*u*v\n");
        // 空参数与 0 都整体关闭；多个参数一次给出。
        assert_eq!(preconvert(b"\x1b[1mx\x1b[m y\n"), "**x** y\n");
        assert_eq!(preconvert(b"\x1b[1;4mx\x1b[0m y\n"), "***x*** y\n");
    }

    #[test]
    fn 预转换_块尾未关闭的样式补闭标记() {
        assert_eq!(preconvert(b"\x1b[1mno close"), "**no close**");
        assert_eq!(preconvert(b"\x1b[4mno close\n"), "*no close*\n");
    }

    #[test]
    fn 预转换_退格叠加() {
        // X\bX 粗体、X\b_ 与 _\bX 下划线；相邻同样式合并成一个跨度。
        assert_eq!(preconvert(b"v\x08va\x08at\x08t\n"), "**vat**\n");
        assert_eq!(preconvert(b"u\x08_d\x08_e\x08_r\x08_\n"), "*uder*\n");
        assert_eq!(preconvert(b"_\x08q tail\n"), "*q* tail\n");
        // 三重叠加只显示一次。
        assert_eq!(
            preconvert(b"k\x08k\x08ke\x08e\x08ey\x08y\x08y\n"),
            "**key**\n"
        );
    }

    #[test]
    fn 预转换_其余控制序列剥除() {
        // 非 m 终结的 CSI、其他 C0 控制字符剥除；换行与制表符保留。
        assert_eq!(preconvert(b"\x1b[2Ktext\x07\r\n"), "text\n");
        assert_eq!(preconvert(b"a\x1b(Bb\x1b7c\n"), "abc\n");
        assert_eq!(preconvert(b"col1\tcol2\n"), "col1\tcol2\n");
        // 文本逐字节保留。
        assert_eq!(
            preconvert(b"plain  spaced --  dashes\x08\n"),
            "plain  spaced --  dashes\n"
        );
    }

    #[test]
    fn 预转换_sgr_与退格叠加混排() {
        assert_eq!(
            preconvert(b"\x1b[1mB\x08Bold\x1b[0m rest\n"),
            "**Bold** rest\n"
        );
        // SGR 粗体里的下划线叠加：粗体跨度内嵌下划线。
        assert_eq!(preconvert(b"\x1b[1ma\x08_b\x1b[0m\n"), "***a*b**\n");
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
