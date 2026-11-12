//! 模型返回的加工与结构校验（规格 §7.4、§7.5）。
//!
//! 加工只有两种：剥掉整块 Markdown 围栏、剥掉首尾纯空行。
//! 校验只看结构与样式位闭合，不比较长度、行数或语义。

/// 结构非法的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// 孤悬退格。
    LoneBackspace,
    /// 非法 SGR（含块结束时仍有样式位未关闭）。
    Sgr,
    /// 输出里出现可见的 `ESC[` 或 `\x08` 字样。
    LiteralEscape,
}

/// 先剥整块围栏，再剥首尾纯空行（§7.4）。
///
/// 围栏之外不做任何剥离；行内与行尾空格保留。
pub fn postprocess(raw: &str) -> String {
    let text = strip_fence(raw).unwrap_or_else(|| raw.to_string());
    strip_blank_edges(&text)
}

/// 整段响应是否恰好是一个 Markdown 围栏块。
fn strip_fence(text: &str) -> Option<String> {
    let (body, had_trailing_newline) = match text.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (text, false),
    };
    let mut lines = body.split('\n');
    let first = lines.next()?;
    if !first.starts_with("```") {
        return None;
    }
    let rest: Vec<&str> = lines.collect();
    let (last, middle) = rest.split_last()?;
    if last.trim() != "```" || middle.iter().any(|line| line.contains('`')) {
        return None;
    }
    let mut out = middle.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    Some(out)
}

/// 剥掉首尾只含空白的行；中间的行原样保留。
fn strip_blank_edges(text: &str) -> String {
    let lines = lines_with_endings(text);
    let Some(first) = lines.iter().position(|line| !is_blank_line(content(line))) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !is_blank_line(content(line)))
        .unwrap_or(first);
    lines[first..=last].concat()
}

/// 按 `\n` 切行，除最后一行外都带换行符。
fn lines_with_endings(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(offset) = rest.find('\n') {
        lines.push(&rest[..=offset]);
        rest = &rest[offset + 1..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

fn content(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

fn is_blank_line(text: &str) -> bool {
    text.chars().all(|c| matches!(c, ' ' | '\t' | '\r'))
}

/// 输出是否含样式序列（SGR 或退格叠加）。
pub fn has_styles(text: &str) -> bool {
    text.as_bytes().iter().any(|&b| b == 0x1b || b == 0x08)
}

/// §7.5 的三条结构判定。
pub fn structural(text: &str) -> Result<(), Invalid> {
    if text.contains("ESC[") || text.contains("\\x08") {
        return Err(Invalid::LiteralEscape);
    }
    let bytes = text.as_bytes();
    // 常见样式位：1 2 3 4 7 8 9，位序与数组下标一致。
    let mut open: u8 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                if bytes.get(i + 1) != Some(&b'[') {
                    return Err(Invalid::Sgr);
                }
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                    j += 1;
                }
                if j >= bytes.len() || bytes[j] != b'm' {
                    return Err(Invalid::Sgr);
                }
                apply_sgr(&bytes[start..j], &mut open);
                i = j + 1;
            }
            0x08 => {
                let prev_ok = i > 0 && is_printable(bytes[i - 1]);
                let next_ok = bytes.get(i + 1).is_some_and(|&b| is_printable(b));
                if !prev_ok || !next_ok {
                    return Err(Invalid::LoneBackspace);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    if open != 0 {
        return Err(Invalid::Sgr);
    }
    Ok(())
}

fn is_printable(byte: u8) -> bool {
    byte >= 0x20 && byte != 0x7f
}

fn style_bit(param: u16) -> Option<u8> {
    let index = match param {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        7 => 4,
        8 => 5,
        9 => 6,
        _ => return None,
    };
    Some(1 << index)
}

fn apply_sgr(params: &[u8], open: &mut u8) {
    let text = std::str::from_utf8(params).unwrap_or("");
    if text.is_empty() {
        *open = 0;
        return;
    }
    for part in text.split(';') {
        let value = part.parse::<u16>().unwrap_or(0);
        match value {
            0 => *open = 0,
            22 => *open &= !(style_bit(1).unwrap() | style_bit(2).unwrap()),
            23 => *open &= !style_bit(3).unwrap(),
            24 => *open &= !style_bit(4).unwrap(),
            27 => *open &= !style_bit(7).unwrap(),
            28 => *open &= !style_bit(8).unwrap(),
            29 => *open &= !style_bit(9).unwrap(),
            _ => {
                if let Some(bit) = style_bit(value) {
                    *open |= bit;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 剥整块围栏() {
        assert_eq!(postprocess("```\nabc\n```"), "abc");
        assert_eq!(postprocess("```text\nabc\ndef\n```\n"), "abc\ndef\n");
        // 围栏内出现反引号时不剥。
        assert_eq!(postprocess("```\na`b\n```"), "```\na`b\n```");
        // 只有起始围栏不剥。
        assert_eq!(postprocess("```\nabc"), "```\nabc");
        // 首行不以 ``` 开头不剥。
        assert_eq!(postprocess("x\n```\nabc\n```"), "x\n```\nabc\n```");
    }

    #[test]
    fn 剥首尾纯空行但保留行尾空格() {
        assert_eq!(postprocess("\n\nabc\n\n\n"), "abc\n");
        assert_eq!(postprocess("\n  \n\t\nabc  \n   \n"), "abc  \n");
        assert_eq!(postprocess("  \n"), "");
        assert_eq!(postprocess(""), "");
        // 末行没有换行符时不补。
        assert_eq!(postprocess("\nabc"), "abc");
        assert_eq!(postprocess("a\n\nb\n"), "a\n\nb\n");
    }

    #[test]
    fn 围栏外多一个空行就不算整块围栏() {
        assert_eq!(postprocess("```\nabc\n```\n"), "abc\n");
        assert_eq!(postprocess("\n```\nabc\n```"), "```\nabc\n```");
    }

    #[test]
    fn 原样回显合法() {
        let input = "NAME\n     ls - list\n";
        assert_eq!(postprocess(input), input);
        assert_eq!(structural(input), Ok(()));
    }

    #[test]
    fn 合法_sgr_与退格叠加() {
        for text in [
            "\x1b[1mbold\x1b[0m",
            "\x1b[1;4mstyled\x1b[22m\x1b[24m",
            "\x1b[3mitalic\x1b[23m",
            "\x1b[4munder\x1b[24m",
            "\x1b[7mrev\x1b[27m",
            "\x1b[9mstrike\x1b[29m",
            "\x1b[1m\x1b[m",
            "X\x08X",
            "X\x08_",
            "指标\x1b[1m加粗\x1b[0m",
        ] {
            assert_eq!(structural(text), Ok(()), "{text:?}");
        }
    }

    #[test]
    fn 孤悬退格非法() {
        for text in ["\x08X", "X\x08", "a\n\x08X\n", "X\x08\n", "X\x08\x08Y"] {
            assert_eq!(structural(text), Err(Invalid::LoneBackspace), "{text:?}");
        }
    }

    #[test]
    fn 非法_sgr_与未闭合样式位() {
        for text in [
            "\x1bX",
            "\x1b[1x",
            "\x1b[1",
            "\x1b[1mnever closed",
            "\x1b[4munder not closed",
        ] {
            assert_eq!(structural(text), Err(Invalid::Sgr), "{text:?}");
        }
        // 22 关闭 1 与 2；31 不是被跟踪的位。
        assert_eq!(structural("\x1b[1;31mtext\x1b[22m"), Ok(()));
        assert_eq!(structural("\x1b[31m red \x1b[0m"), Ok(()));
    }

    #[test]
    fn 字面转义非法() {
        assert_eq!(structural("text ESC[1m more"), Err(Invalid::LiteralEscape));
        assert_eq!(structural(r"text \x08 more"), Err(Invalid::LiteralEscape));
    }

    #[test]
    fn 样式序列判定() {
        assert!(has_styles("\x1b[1mx\x1b[0m"));
        assert!(has_styles("X\x08X"));
        assert!(!has_styles("plain"));
        assert!(!has_styles("ESC[1m"));
    }
}
