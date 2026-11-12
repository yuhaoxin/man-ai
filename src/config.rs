//! 配置文件（规格 §10.1、§10.2）：XDG 路径、逐键校验、告警文案。
//!
//! 逐键解析而不是整体反序列化：单个坏键只影响该键，其余照常生效。
//! 告警只收集到 `LoadedConfig`，由调用方决定输出位置。

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::language::Language;

/// §10.2 全键。必填项缺失保持 `None`（= not configured），其余为默认值。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 归一后的规范语言标签；空或非法时为 `None`。
    pub lang: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub timeout_seconds: f64,
    pub concurrency: usize,
    pub cache_enabled: bool,
    pub cache_max_mb: u64,
    pub proxy: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lang: None,
            endpoint: None,
            model: None,
            api_key: None,
            api_key_env: None,
            max_tokens: None,
            timeout_seconds: 30.0,
            concurrency: 4,
            cache_enabled: true,
            cache_max_mb: 200,
            proxy: None,
        }
    }
}

pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// `$XDG_CONFIG_HOME/man-ai/config.toml`；未设或为空时 `$HOME/.config/...`。
pub fn config_path() -> Option<PathBuf> {
    let xdg = env::var("XDG_CONFIG_HOME").ok();
    let home = env::var("HOME").ok();
    config_path_from(xdg.as_deref(), home.as_deref())
}

fn config_path_from(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = non_empty(xdg)
        .map(PathBuf::from)
        .or_else(|| non_empty(home).map(|home| Path::new(home).join(".config")))?;
    Some(base.join("man-ai").join("config.toml"))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// 读文件并逐键校验。缺文件静默；任何问题都只回退默认值，不改退出码。
pub fn load(path: Option<&Path>) -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config: Config::default(),
        warnings: Vec::new(),
    };
    let Some(path) = path else {
        return loaded;
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return loaded,
        // 读不出来与语法错误一样按「整个文件无效」处理。
        Err(_) => {
            loaded.warnings.push(parse_warning(path));
            return loaded;
        }
    };
    let table = match toml::from_str::<Value>(&text) {
        Ok(Value::Table(table)) => table,
        _ => {
            loaded.warnings.push(parse_warning(path));
            return loaded;
        }
    };

    for (key, value) in table {
        match key.as_str() {
            "lang" => {
                let Some(raw) = value.as_str() else {
                    invalid(&mut loaded, "lang");
                    continue;
                };
                match Language::from_config(raw) {
                    Some(lang) => loaded.config.lang = Some(lang.tag().to_string()),
                    None if raw.trim().is_empty() => {}
                    None => loaded
                        .warnings
                        .push(format!("man-ai: ignoring invalid config key lang: {raw}")),
                }
            }
            "endpoint" => match string_value(&value) {
                Some(raw) if valid_endpoint(raw) => {
                    loaded.config.endpoint = Some(raw.to_string());
                }
                Some(_) => invalid(&mut loaded, "endpoint"),
                None => {}
            },
            "model" => loaded.config.model = string_value(&value).map(str::to_string),
            "api_key" => loaded.config.api_key = string_value(&value).map(str::to_string),
            "api_key_env" => loaded.config.api_key_env = string_value(&value).map(str::to_string),
            "max_tokens" => match value.as_integer().and_then(|v| u32::try_from(v).ok()) {
                Some(tokens) if tokens > 0 => loaded.config.max_tokens = Some(tokens),
                _ => invalid(&mut loaded, "max_tokens"),
            },
            "timeout_seconds" => {
                let seconds = value
                    .as_float()
                    .or_else(|| value.as_integer().map(|v| v as f64));
                match seconds {
                    Some(seconds) if seconds > 0.0 => loaded.config.timeout_seconds = seconds,
                    _ => invalid(&mut loaded, "timeout_seconds"),
                }
            }
            "concurrency" => match value.as_integer() {
                Some(v) if v >= 1 => loaded.config.concurrency = v as usize,
                _ => invalid(&mut loaded, "concurrency"),
            },
            "cache_enabled" => match value.as_bool() {
                Some(enabled) => loaded.config.cache_enabled = enabled,
                None => invalid(&mut loaded, "cache_enabled"),
            },
            "cache_max_mb" => match value.as_integer() {
                Some(v) if v >= 0 => loaded.config.cache_max_mb = v as u64,
                _ => invalid(&mut loaded, "cache_max_mb"),
            },
            "proxy" => match string_value(&value) {
                Some(raw) if valid_proxy(raw) => loaded.config.proxy = Some(raw.to_string()),
                Some(_) => invalid(&mut loaded, "proxy"),
                None => {}
            },
            _ => loaded
                .warnings
                .push(format!("man-ai: ignoring unknown config key {key}")),
        }
    }
    loaded
}

fn parse_warning(path: &Path) -> String {
    format!(
        "man-ai: failed to parse config ({}); using defaults",
        path.display()
    )
}

fn invalid(loaded: &mut LoadedConfig, key: &str) {
    loaded
        .warnings
        .push(format!("man-ai: ignoring invalid config key {key}"));
}

/// 空串与纯空白视为未设置。
fn string_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// §10.2：只接受 http/https、必须有 host、不含查询串与片段。
fn valid_endpoint(raw: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

/// §10.2：只接受 http/https/socks5。
fn valid_proxy(raw: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https" | "socks5") && url.host().is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = env::temp_dir().join(format!(
                "man-ai-config-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn load_toml(dir: &TempDir, text: &str) -> LoadedConfig {
        let path = dir.file("config.toml");
        fs::write(&path, text).unwrap();
        load(Some(&path))
    }

    const FULL: &str = r#"
lang = "zh_CN.UTF-8"
endpoint = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key = "sk-test"
api_key_env = "DEEPSEEK_API_KEY"
max_tokens = 4096
timeout_seconds = 12.5
concurrency = 8
cache_enabled = false
cache_max_mb = 50
proxy = "socks5://127.0.0.1:1080"
"#;

    #[test]
    fn 缺文件与缺路径静默取默认值() {
        let dir = TempDir::new("missing");
        let missing = load(Some(&dir.file("nope.toml")));
        assert!(missing.warnings.is_empty(), "{:?}", missing.warnings);
        assert_eq!(missing.config, Config::default());

        let none = load(None);
        assert!(none.warnings.is_empty());
        assert_eq!(none.config, Config::default());
    }

    #[test]
    fn 默认值按规格() {
        let config = Config::default();
        assert_eq!(config.lang, None);
        assert_eq!(config.endpoint, None);
        assert_eq!(config.model, None);
        assert_eq!(config.max_tokens, None);
        assert_eq!(config.timeout_seconds, 30.0);
        assert_eq!(config.concurrency, 4);
        assert!(config.cache_enabled);
        assert_eq!(config.cache_max_mb, 200);
        assert_eq!(config.proxy, None);
    }

    #[test]
    fn 语法错误告警并整体按默认值() {
        let dir = TempDir::new("syntax");
        let loaded = load_toml(&dir, "endpoint = [unclosed\nlang = \"ja\"\n");
        assert_eq!(
            loaded.warnings,
            vec![format!(
                "man-ai: failed to parse config ({}); using defaults",
                dir.file("config.toml").display()
            )]
        );
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn 未知键告警其余键照常生效() {
        let dir = TempDir::new("unknown");
        let loaded = load_toml(
            &dir,
            "endpoint = \"https://api.example.com/v1\"\ncolour = \"blue\"\n",
        );
        assert_eq!(
            loaded.warnings,
            vec!["man-ai: ignoring unknown config key colour".to_string()]
        );
        assert_eq!(
            loaded.config.endpoint.as_deref(),
            Some("https://api.example.com/v1")
        );
    }

    #[test]
    fn 单键类型不符与越界告警并按默认值() {
        let dir = TempDir::new("bad");
        let loaded = load_toml(
            &dir,
            "concurrency = \"four\"\n\
             max_tokens = 0\n\
             timeout_seconds = 0\n\
             cache_max_mb = -1\n\
             cache_enabled = \"yes\"\n",
        );
        let mut warnings = loaded.warnings.clone();
        warnings.sort();
        assert_eq!(
            warnings,
            vec![
                "man-ai: ignoring invalid config key cache_enabled",
                "man-ai: ignoring invalid config key cache_max_mb",
                "man-ai: ignoring invalid config key concurrency",
                "man-ai: ignoring invalid config key max_tokens",
                "man-ai: ignoring invalid config key timeout_seconds",
            ]
        );
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn endpoint_只接受_http_https_且无查询与片段() {
        let dir = TempDir::new("endpoint");
        for raw in [
            "ftp://example.com/v1",
            "https://example.com/v1?key=1",
            "https://example.com/v1#frag",
            "example.com/v1",
            "https://",
        ] {
            let loaded = load_toml(&dir, &format!("endpoint = \"{raw}\"\n"));
            assert_eq!(
                loaded.warnings,
                vec!["man-ai: ignoring invalid config key endpoint".to_string()],
                "{raw}"
            );
            assert_eq!(loaded.config.endpoint, None, "{raw}");
        }
        for raw in ["http://127.0.0.1:11434/v1", "https://api.example.com/v1"] {
            let loaded = load_toml(&dir, &format!("endpoint = \"{raw}\"\n"));
            assert!(loaded.warnings.is_empty(), "{raw}: {:?}", loaded.warnings);
            assert_eq!(loaded.config.endpoint.as_deref(), Some(raw));
        }
    }

    #[test]
    fn proxy_只接受_http_https_socks5() {
        let dir = TempDir::new("proxy");
        for raw in ["ftp://p:1", "socks4://p:1", "127.0.0.1:1080"] {
            let loaded = load_toml(&dir, &format!("proxy = \"{raw}\"\n"));
            assert_eq!(
                loaded.warnings,
                vec!["man-ai: ignoring invalid config key proxy".to_string()],
                "{raw}"
            );
            assert_eq!(loaded.config.proxy, None);
        }
        for raw in [
            "http://127.0.0.1:8080",
            "https://proxy.example.com",
            "socks5://127.0.0.1:1080",
        ] {
            let loaded = load_toml(&dir, &format!("proxy = \"{raw}\"\n"));
            assert!(loaded.warnings.is_empty(), "{raw}: {:?}", loaded.warnings);
            assert_eq!(loaded.config.proxy.as_deref(), Some(raw));
        }
    }

    #[test]
    fn lang_非法告警并回退_空值不告警() {
        let dir = TempDir::new("lang");
        let loaded = load_toml(&dir, "lang = \"zh CN\"\n");
        assert_eq!(
            loaded.warnings,
            vec!["man-ai: ignoring invalid config key lang: zh CN".to_string()]
        );
        assert_eq!(loaded.config.lang, None);

        let loaded = load_toml(&dir, "lang = \"\"\n");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.lang, None);

        let loaded = load_toml(&dir, "lang = \"zh_CN.UTF-8\"\n");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.lang.as_deref(), Some("zh-CN"));
    }

    #[test]
    fn 全量合法配置按_规格读入() {
        let dir = TempDir::new("full");
        let loaded = load_toml(&dir, FULL);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let config = loaded.config;
        assert_eq!(config.lang.as_deref(), Some("zh-CN"));
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(config.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(config.max_tokens, Some(4096));
        assert_eq!(config.timeout_seconds, 12.5);
        assert_eq!(config.concurrency, 8);
        assert!(!config.cache_enabled);
        assert_eq!(config.cache_max_mb, 50);
        assert_eq!(config.proxy.as_deref(), Some("socks5://127.0.0.1:1080"));
    }

    #[test]
    fn 整数形式的超时时间也接受() {
        let dir = TempDir::new("timeout-int");
        let loaded = load_toml(&dir, "timeout_seconds = 30\n");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.timeout_seconds, 30.0);
    }

    #[test]
    fn 配置路径按_xdg_与_home_回退() {
        assert_eq!(
            config_path_from(Some("/xdg"), Some("/home/u")),
            Some(PathBuf::from("/xdg/man-ai/config.toml"))
        );
        assert_eq!(
            config_path_from(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/man-ai/config.toml"))
        );
        assert_eq!(
            config_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/man-ai/config.toml"))
        );
        assert_eq!(
            config_path_from(Some("/xdg"), None),
            Some(PathBuf::from("/xdg/man-ai/config.toml"))
        );
        assert_eq!(config_path_from(None, None), None);
    }
}
