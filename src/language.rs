//! 语言标签的纯字符串归一化与 `--lang` 形状校验（规格 §11.2、§11.6）。
//!
//! 本票只有 CLI 形状校验；来源链、显示名与提示词名由 #19 扩展。

/// 剥掉 `.` 起的编码部分与 `@` 起的修饰符。
fn strip_locale(raw: &str) -> &str {
    let base = raw.split('.').next().unwrap_or(raw);
    base.split('@').next().unwrap_or(base)
}

/// §11.2 第 1、3 步：纯字符串归一化。非法字符返回 `None`。
fn canonicalize(raw: &str) -> Option<String> {
    let base = strip_locale(raw.trim());
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Some("en".to_string());
    }
    if !base.is_ascii() {
        return None;
    }
    let mut tag = String::with_capacity(base.len());
    for (index, part) in base.split(['_', '-']).enumerate() {
        if part.is_empty() {
            return None;
        }
        if index > 0 {
            tag.push('-');
        }
        if index == 0 {
            tag.push_str(&part.to_ascii_lowercase());
        } else if part.len() == 4 {
            let lower = part.to_ascii_lowercase();
            tag.push_str(&lower[..1].to_ascii_uppercase());
            tag.push_str(&lower[1..]);
        } else if part.len() == 2 || (part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit())) {
            tag.push_str(&part.to_ascii_uppercase());
        } else {
            tag.push_str(part);
        }
    }
    Some(tag)
}

/// §11.6：`--lang` 的形状校验，返回规范标签或指向 `--help` 的报错文案。
pub fn cli_tag(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--lang value is empty; see man-ai --help".to_string());
    }
    let tag = canonicalize(trimmed)
        .ok_or_else(|| "invalid language tag; see man-ai --help".to_string())?;
    let well_formed = !tag.starts_with('-')
        && !tag.ends_with('-')
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if well_formed {
        Ok(tag)
    } else {
        Err("invalid language tag; see man-ai --help".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 归一化环境形态() {
        assert_eq!(cli_tag("zh_CN.UTF-8").unwrap(), "zh-CN");
        assert_eq!(cli_tag("zh_CN.UTF-8@euro").unwrap(), "zh-CN");
        assert_eq!(cli_tag("zh_hant_tw").unwrap(), "zh-Hant-TW");
        assert_eq!(cli_tag("pt_br").unwrap(), "pt-BR");
        assert_eq!(cli_tag("ZH").unwrap(), "zh");
        assert_eq!(cli_tag("es-419").unwrap(), "es-419");
    }

    #[test]
    fn 已是规范标签时幂等() {
        for tag in ["zh-CN", "zh-Hant-TW", "ja", "pt-BR", "en"] {
            assert_eq!(cli_tag(tag).unwrap(), tag);
        }
    }

    #[test]
    fn c_与_posix_视为英语() {
        assert_eq!(cli_tag("C").unwrap(), "en");
        assert_eq!(cli_tag("POSIX").unwrap(), "en");
    }

    #[test]
    fn 空值与形状非法报错() {
        for raw in ["", "   ", "zh CN", "中文", "-zh", "zh-", "zh--CN"] {
            assert!(cli_tag(raw).is_err(), "{raw:?} 应被拒绝");
        }
        assert!(cli_tag("").unwrap_err().contains("man-ai --help"));
    }
}
