//! 语言标签的归一化、来源链、显示名与提示词英文名（规格 §11.1–§11.6）。
//!
//! 纯字符串逻辑集中在这里：环境访问只经 `resolve_with` 传入的闭包，测试不碰进程环境。

use std::env;

/// 剥掉 `.` 起的编码部分与 `@` 起的修饰符。
fn strip_locale(raw: &str) -> &str {
    let base = raw.split('.').next().unwrap_or(raw);
    base.split('@').next().unwrap_or(base)
}

/// §11.2：归一化为 `^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$` 形态，非法返回 `None`。
///
/// `C`、`POSIX` 与空值归一为 `en`；只做纯字符串校正，不做 CLDR 推断。
pub fn normalize(raw: &str) -> Option<String> {
    let base = strip_locale(raw.trim());
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Some("en".to_string());
    }
    if !base.is_ascii() {
        return None;
    }
    let mut tag = String::with_capacity(base.len());
    for (index, part) in base.split(['_', '-']).enumerate() {
        if part.is_empty() || part.len() > 8 {
            return None;
        }
        if index == 0 {
            let primary_ok = part.len() >= 2 && part.bytes().all(|b| b.is_ascii_alphabetic());
            if !primary_ok {
                return None;
            }
            tag.push_str(&part.to_ascii_lowercase());
            continue;
        }
        if !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        tag.push('-');
        if part.len() == 4 {
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

/// 生效语言：归一后的规范标签，外加显示名与提示词英文名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Language {
    tag: String,
}

impl Language {
    /// §11.6：`--lang` 的值。空值或形状非法都是用法错误，文案指向 `--help`。
    pub fn parse_cli(raw: &str) -> Result<Self, String> {
        if raw.trim().is_empty() {
            return Err("--lang value is empty; see man-ai --help".to_string());
        }
        normalize(raw)
            .map(|tag| Self { tag })
            .ok_or_else(|| "invalid language tag; see man-ai --help".to_string())
    }

    /// 配置文件的 `lang`：非法或空值返回 `None`，由调用方决定是否告警。
    pub fn from_config(raw: &str) -> Option<Self> {
        if raw.trim().is_empty() {
            return None;
        }
        normalize(raw).map(|tag| Self { tag })
    }

    /// §11.1：CLI > 配置 > `LC_ALL` > `LC_MESSAGES` > `LANG` > `en`。
    ///
    /// 环境值非法静默跳过；`LANGUAGE` 不参与。
    pub fn resolve(cli: Option<Self>, config: Option<Self>) -> Self {
        resolve_with(cli, config, |key| env::var(key).ok())
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// §11.3：主标签为 `en` 时不翻译。
    pub fn is_english(&self) -> bool {
        self.tag
            .split('-')
            .next()
            .is_some_and(|primary| primary.eq_ignore_ascii_case("en"))
    }

    /// §11.4：页首/尾行里的 `<language>`。
    pub fn display(&self) -> &str {
        self.name()
    }

    /// §11.4：提示词里的 `{TARGET_LANGUAGE}`。
    pub fn prompt_name(&self) -> &str {
        self.name()
    }

    /// 精确匹配 §11.4 表；表外标签用标签原文，不做前缀回退。
    fn name(&self) -> &str {
        match self.tag.as_str() {
            "zh-CN" | "zh-Hans" => "Simplified Chinese",
            "zh-SG" => "Simplified Chinese (Singapore)",
            "zh-TW" | "zh-Hant" => "Traditional Chinese",
            "zh-HK" => "Traditional Chinese (Hong Kong)",
            "ja" => "Japanese",
            "ko" => "Korean",
            "en" => "English",
            "fr" => "French",
            "de" => "German",
            "es" => "Spanish",
            "pt" => "Portuguese",
            "pt-BR" => "Brazilian Portuguese",
            "pt-PT" => "European Portuguese",
            "ru" => "Russian",
            "it" => "Italian",
            "nl" => "Dutch",
            "pl" => "Polish",
            "tr" => "Turkish",
            "uk" => "Ukrainian",
            "vi" => "Vietnamese",
            "th" => "Thai",
            "id" => "Indonesian",
            "ms" => "Malay",
            "ar" => "Arabic",
            "he" => "Hebrew",
            "fa" => "Persian",
            "hi" => "Hindi",
            "cs" => "Czech",
            "sv" => "Swedish",
            "da" => "Danish",
            "fi" => "Finnish",
            "nb" => "Norwegian Bokmål",
            "el" => "Greek",
            "hu" => "Hungarian",
            "ro" => "Romanian",
            _ => &self.tag,
        }
    }
}

fn resolve_with(
    cli: Option<Language>,
    config: Option<Language>,
    env: impl Fn(&str) -> Option<String>,
) -> Language {
    if let Some(lang) = cli.or(config) {
        return lang;
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = env(key) {
            if value.trim().is_empty() {
                continue;
            }
            if let Some(tag) = normalize(&value) {
                return Language { tag };
            }
        }
    }
    Language {
        tag: "en".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(raw: &str) -> Language {
        Language::parse_cli(raw).expect("合法标签")
    }

    #[test]
    fn 归一化环境形态() {
        assert_eq!(normalize("zh_CN.UTF-8").as_deref(), Some("zh-CN"));
        assert_eq!(normalize("zh_CN.UTF-8@euro").as_deref(), Some("zh-CN"));
        assert_eq!(normalize("zh_hant_tw").as_deref(), Some("zh-Hant-TW"));
        assert_eq!(normalize("pt_br").as_deref(), Some("pt-BR"));
        assert_eq!(normalize("ZH").as_deref(), Some("zh"));
        assert_eq!(normalize("es-419").as_deref(), Some("es-419"));
        assert_eq!(normalize("en_US.UTF-8").as_deref(), Some("en-US"));
        assert_eq!(normalize("sr_Latn_RS").as_deref(), Some("sr-Latn-RS"));
    }

    #[test]
    fn c_与_posix_与空值视为英语() {
        for raw in ["C", "POSIX", "", "  ", "C.UTF-8", "POSIX.UTF-8"] {
            assert_eq!(normalize(raw).as_deref(), Some("en"), "{raw:?}");
        }
    }

    #[test]
    fn 形状非法返回_none() {
        for raw in ["zh CN", "中文", "-zh", "zh-", "zh--CN", "x", "1", "zh_"] {
            assert_eq!(normalize(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn 规范标签幂等() {
        for tag in [
            "zh-CN",
            "zh-Hant-TW",
            "ja",
            "pt-BR",
            "en",
            "es-419",
            "sr-Latn-RS",
        ] {
            assert_eq!(normalize(tag).as_deref(), Some(tag));
            assert_eq!(lang(tag).tag(), tag);
        }
    }

    #[test]
    fn parse_cli_报错指向_help() {
        for raw in ["", "   ", "zh CN", "中文", "-zh", "zh-", "zh--CN"] {
            let err = Language::parse_cli(raw).unwrap_err();
            assert!(err.contains("man-ai --help"), "{raw:?}: {err}");
        }
    }

    #[test]
    fn from_config_空值与非法值返回_none() {
        assert_eq!(
            Language::from_config(" zh_CN.UTF-8 ").map(|l| l.tag().to_string()),
            Some("zh-CN".to_string())
        );
        for raw in ["", "   ", "zh CN", "中文"] {
            assert!(Language::from_config(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn 来源链_cli_高于配置_配置高于环境() {
        let env = |key: &str| (key == "LC_ALL").then(|| "fr_FR.UTF-8".to_string());
        assert_eq!(
            resolve_with(Some(lang("ja")), Some(lang("ko")), env).tag(),
            "ja"
        );
        assert_eq!(resolve_with(None, Some(lang("ko")), env).tag(), "ko");
        assert_eq!(resolve_with(None, None, env).tag(), "fr-FR");
    }

    #[test]
    fn 环境链_lc_all_优先于_lc_messages_优先于_lang() {
        let env = |key: &str| match key {
            "LC_ALL" => Some("ja_JP.UTF-8".to_string()),
            "LC_MESSAGES" => Some("ko_KR.UTF-8".to_string()),
            "LANG" => Some("de_DE.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(resolve_with(None, None, env).tag(), "ja-JP");

        let env = |key: &str| match key {
            "LC_MESSAGES" => Some("ko_KR.UTF-8".to_string()),
            "LANG" => Some("de_DE.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(resolve_with(None, None, env).tag(), "ko-KR");

        let env = |key: &str| (key == "LANG").then(|| "de_DE.UTF-8".to_string());
        assert_eq!(resolve_with(None, None, env).tag(), "de-DE");
    }

    #[test]
    fn language_变量不参与来源链() {
        let env = |key: &str| (key == "LANGUAGE").then(|| "ja:fr".to_string());
        assert_eq!(resolve_with(None, None, env).tag(), "en");
    }

    #[test]
    fn 环境坏值与空值静默跳到下一个() {
        let env = |key: &str| match key {
            "LC_ALL" => Some("zh CN".to_string()),
            "LC_MESSAGES" => Some(String::new()),
            "LANG" => Some("en_GB.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(resolve_with(None, None, env).tag(), "en-GB");

        let env = |key: &str| (key == "LANG").then(|| "zh CN".to_string());
        assert_eq!(resolve_with(None, None, env).tag(), "en");
    }

    #[test]
    fn 无来源时回落英语() {
        let resolved = resolve_with(None, None, |_| None);
        assert_eq!(resolved.tag(), "en");
        assert!(resolved.is_english());
    }

    #[test]
    fn 主标签为_en_才不翻译() {
        for raw in ["en", "en_US.UTF-8", "en-GB", "POSIX"] {
            assert!(lang(raw).is_english(), "{raw}");
        }
        for raw in ["enm", "eng", "fr", "zh-CN", "ja"] {
            assert!(!lang(raw).is_english(), "{raw}");
        }
    }

    #[test]
    fn 显示名按表精确匹配() {
        assert_eq!(lang("zh-CN").display(), "Simplified Chinese");
        assert_eq!(lang("zh-SG").display(), "Simplified Chinese (Singapore)");
        assert_eq!(lang("zh-TW").display(), "Traditional Chinese");
        assert_eq!(lang("pt-BR").display(), "Brazilian Portuguese");
        assert_eq!(lang("pt").display(), "Portuguese");
        assert_eq!(lang("ja").display(), "Japanese");
        assert_eq!(lang("en").display(), "English");
        assert_eq!(lang("nb").display(), "Norwegian Bokmål");
    }

    #[test]
    fn 表外标签的显示名与提示词名都用标签原文() {
        for tag in ["zh-Hant-TW", "zh", "cmn", "en-GB", "es-419", "zh-Hans-CN"] {
            assert_eq!(lang(tag).display(), tag);
            assert_eq!(lang(tag).prompt_name(), tag);
        }
    }

    #[test]
    fn 提示词名与显示名同表() {
        assert_eq!(
            lang("zh-HK").prompt_name(),
            "Traditional Chinese (Hong Kong)"
        );
        assert_eq!(lang("zh-Hans").prompt_name(), "Simplified Chinese");
        assert_eq!(lang("zh-Hant").prompt_name(), "Traditional Chinese");
        assert_eq!(lang("fi").prompt_name(), "Finnish");
    }
}
