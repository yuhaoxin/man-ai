//! 整页文件缓存（规格 §9）：布局、键字段注释、原子写、命中与剪枝。
//!
//! 只存全部采用后的整页译文；关闭或不可用时整个结构退化为 no-op，
//! 调用方无需分支。命中输出前剥除首行注释。

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::prompt::PROMPT_VERSION;

/// 缓存布局或键字段语义变化时递增（§9.1）。
pub const CACHE_VERSION: u32 = 3;

/// `tmp/` 中滞留超过这个时长的文件在剪枝时删除（§9.5）。
const STALE_TMP: Duration = Duration::from_secs(3600);

/// 键字段中除页名与哈希外的固定部分（§9.1）；`endpoint` 传配置原值，
/// 进注释前规范化。
#[derive(Debug, Clone, Copy)]
pub struct KeyParts<'a> {
    /// 语言规范标签，同时是语言目录名。
    pub lang: &'a str,
    pub endpoint: &'a str,
    pub model: &'a str,
    /// 实际发送的思考强度；`off` 表示配置为空、不发送。
    pub reasoning_effort: &'a str,
}

/// 整页缓存键：页眉解析出的页名与整页预转换文本（§9.1）。
pub struct PageKey {
    name: String,
    section: String,
    /// 哈希对象：全部块 `pre` 按 `gap_before`/`tail_gap` 重组后的字节。
    pre: String,
}

impl PageKey {
    /// 第一行页眉 `NAME(SECTION)` 解析失败时返回 `None`，该页不缓存（§9.1）。
    pub fn parse(pre: String) -> Option<Self> {
        let first = pre.split('\n').next().unwrap_or_default();
        let (name, section) = parse_header(first)?;
        Some(Self { name, section, pre })
    }

    fn file_stem(&self) -> String {
        format!("{}.{}", self.name, self.section)
    }
}

/// 关闭或不可用时 `state` 为 `None`，所有操作 no-op。
pub struct Cache {
    state: Option<State>,
}

struct State {
    root: PathBuf,
    /// 写失败只告警一次；由管线在分页器启动前取走输出。
    warning: Mutex<Option<String>>,
}

impl Cache {
    /// 显式 no-op：不读不写不建目录。
    pub fn disabled() -> Self {
        Self { state: None }
    }

    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    /// 整页命中：返回剥除注释行后的整页译文；读失败或键字段不一致按 miss（§9.3）。
    /// 命中只读，不更新 mtime。
    pub fn get_page(&self, parts: &KeyParts<'_>, key: &PageKey) -> Option<String> {
        let state = self.state.as_ref()?;
        let comment = comment_line(parts, key)?;
        let content = fs::read_to_string(entry_path(&state.root, parts.lang, key)).ok()?;
        Some(
            content
                .strip_prefix(&comment)?
                .strip_prefix('\n')?
                .to_string(),
        )
    }

    /// 整页落盘：注释行 + 译文，覆盖旧条目；失败只记一条告警（§9.2、§9.3）。
    pub fn put_page(&self, parts: &KeyParts<'_>, key: &PageKey, body: &str) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let Some(comment) = comment_line(parts, key) else {
            return;
        };
        let value = format!("{comment}\n{body}");
        let target = entry_path(&state.root, parts.lang, key);
        if write_atomic(&state.root, &target, value.as_bytes()).is_err()
            && let Ok(mut slot) = state.warning.lock()
                && slot.is_none() {
                    *slot = Some("cache write failed; continuing without cache".to_string());
                }
    }

    /// 取走待输出的告警；管线据此在分页器启动前写 stderr（§5.5）。
    pub fn take_warning(&self) -> Option<String> {
        self.state.as_ref()?.warning.lock().ok()?.take()
    }

    /// 删除旧 `v1/` 布局、清陈旧 tmp、按总大小剪枝到上限的 80%（§9.5）。
    /// `0` 表示不限大小。
    pub fn prune(&self, max_mb: u64) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let _ = fs::remove_dir_all(state.root.join("v1"));
        remove_stale_tmp(&state.root.join("tmp"));
        if max_mb == 0 {
            return;
        }
        let limit = max_mb.saturating_mul(1024 * 1024);
        let mut entries = Vec::new();
        let mut total: u64 = 0;
        for lang_dir in fs::read_dir(&state.root).into_iter().flatten().flatten() {
            let path = lang_dir.path();
            if !path.is_dir() || path.file_name().is_some_and(|name| name == "tmp") {
                continue;
            }
            for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                let Ok(meta) = file.metadata() else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                total += meta.len();
                entries.push((modified, meta.len(), file.path()));
            }
        }
        if total <= limit {
            return;
        }
        entries.sort_by_key(|entry| entry.0);
        let target = limit / 100 * 80;
        for (_, size, path) in entries {
            if total <= target {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= size;
            }
        }
    }
}

/// 打开缓存。第二个返回值为一次性告警（英文，调用方加 `man-ai: ` 前缀）。
pub fn open(config: &Config) -> (Cache, Option<String>) {
    if !config.cache_enabled {
        return (Cache::disabled(), None);
    }
    let xdg = std::env::var("XDG_CACHE_HOME").ok();
    let home = std::env::var("HOME").ok();
    open_at(xdg.as_deref(), home.as_deref())
}

fn open_at(xdg: Option<&str>, home: Option<&str>) -> (Cache, Option<String>) {
    let Some(root) = root_path(xdg, home) else {
        return unavailable("XDG_CACHE_HOME and HOME are unset");
    };
    match prepare(&root) {
        Ok(()) => (
            Cache {
                state: Some(State {
                    root,
                    warning: Mutex::new(None),
                }),
            },
            None,
        ),
        Err(detail) => unavailable(&detail),
    }
}

fn unavailable(detail: &str) -> (Cache, Option<String>) {
    (
        Cache::disabled(),
        Some(format!(
            "cache unavailable ({detail}); continuing without cache"
        )),
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// `$XDG_CACHE_HOME/man-ai`，未设置或为空时 `$HOME/.cache/man-ai`。
fn root_path(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = non_empty(xdg) {
        return Some(Path::new(xdg).join("man-ai"));
    }
    non_empty(home).map(|home| Path::new(home).join(".cache").join("man-ai"))
}

/// 建目录、设权限、校验属主与可写性；任何一项失败都降级为 no-op。
fn prepare(root: &Path) -> Result<(), String> {
    let tmp = root.join("tmp");
    fs::create_dir_all(&tmp).map_err(|error| error.to_string())?;
    for dir in [root, &tmp] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let owner = fs::metadata(root).map_err(|error| error.to_string())?.uid();
    // SAFETY: geteuid 无参数、无副作用。
    if owner != unsafe { libc::geteuid() } {
        return Err("cache directory belongs to another user".to_string());
    }
    // 已有目录的只读只有真写一次才看得出；探针文件随即删除。
    tempfile::Builder::new()
        .prefix(&format!("{}-", std::process::id()))
        .tempfile_in(&tmp)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// `<语言规范标签>/<页名>.<section>.md`（§9.1）。
fn entry_path(root: &Path, lang: &str, key: &PageKey) -> PathBuf {
    root.join(lang).join(format!("{}.md", key.file_stem()))
}

/// 同目录 `tmp/` 写完再 `rename`，语言目录按需创建并设 `0700`（§9.2）。
fn write_atomic(root: &Path, target: &Path, value: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{}-", std::process::id()))
        .tempfile_in(root.join("tmp"))?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(value)?;
    file.persist(target).map_err(|error| error.error)?;
    Ok(())
}

fn remove_stale_tmp(tmp: &Path) {
    let now = SystemTime::now();
    for file in fs::read_dir(tmp).into_iter().flatten().flatten() {
        let Ok(meta) = file.metadata() else {
            continue;
        };
        let stale = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_TMP);
        if stale {
            let _ = fs::remove_file(file.path());
        }
    }
}

/// 首行注释：键字段以单个空格分隔，值中空格转 `%20`（§9.1）。
/// 端点无法规范化时返回 `None`，该页不读不写。
fn comment_line(parts: &KeyParts<'_>, key: &PageKey) -> Option<String> {
    let endpoint = normalize_endpoint(parts.endpoint)?;
    Some(format!(
        "<!-- v={} prompt={} endpoint={} model={} reasoning={} hash={} -->",
        CACHE_VERSION,
        PROMPT_VERSION,
        escape(&endpoint),
        escape(parts.model),
        escape(parts.reasoning_effort),
        hex16(blake3::hash(key.pre.as_bytes()).as_bytes()),
    ))
}

fn escape(value: &str) -> String {
    value.replace(' ', "%20")
}

fn hex16(digest: &[u8; 32]) -> String {
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 页眉 `NAME(SECTION)`：页名转小写；页名与 section 只接受文件名安全字符，
/// 含其它字符（空格、路径分隔符等）视为解析失败（§9.1）。
fn parse_header(line: &str) -> Option<(String, String)> {
    // 预转换把粗体页眉转成 `**NAME(SECTION)**`，先剥掉强调标记。
    let line = line.trim().trim_matches('*').trim();
    let body = line.strip_suffix(')')?;
    let open = body.rfind('(')?;
    let (name, section) = (&body[..open], &body[open + 1..]);
    let safe =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | ':' | '~' | '-');
    if name.is_empty() || section.is_empty() {
        return None;
    }
    if !name.chars().all(safe) || !section.chars().all(safe) {
        return None;
    }
    Some((name.to_ascii_lowercase(), section.to_string()))
}

/// scheme/host 小写、去默认端口、去尾部 `/`；query 或 fragment 不接受（§9.1）。
pub fn normalize_endpoint(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    let usable = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.query().is_none()
        && url.fragment().is_none();
    if !usable {
        return None;
    }
    Some(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;

    fn parts<'a>(lang: &'a str, endpoint: &'a str, model: &'a str) -> KeyParts<'a> {
        KeyParts {
            lang,
            endpoint,
            model,
            reasoning_effort: "low",
        }
    }

    fn temp(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(tag).tempdir().unwrap()
    }

    fn cache_in(dir: &tempfile::TempDir) -> Cache {
        let (cache, warning) = open_at(dir.path().to_str(), None);
        assert_eq!(warning, None);
        cache
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
    }

    fn put_entry(dir: &tempfile::TempDir, lang: &str, stem: &str, size: usize) -> PathBuf {
        let path = dir
            .path()
            .join("man-ai")
            .join(lang)
            .join(format!("{stem}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; size]).unwrap();
        path
    }

    #[test]
    fn 页眉解析() {
        let table = [
            ("LS(1)", Some(("ls", "1"))),
            ("**LS(1)**", Some(("ls", "1"))),
            ("Curl-Config(1)", Some(("curl-config", "1"))),
            ("x509(1ssl)", Some(("x509", "1ssl"))),
            ("mwm(1x)", Some(("mwm", "1x"))),
            ("LS (1)", None),
            ("ls", None),
            ("(1)", None),
            ("ls()", None),
            ("a b(1)", None),
            ("../evil(1)", None),
            ("a(b)(1)", None),
        ];
        for (line, expected) in table {
            let expected = expected.map(|(name, section)| (name.to_string(), section.to_string()));
            assert_eq!(parse_header(line), expected, "{line}");
        }
    }

    #[test]
    fn 无页眉或首行为空的页不缓存() {
        assert!(PageKey::parse(String::new()).is_none());
        assert!(PageKey::parse("\nLS(1)\n".to_string()).is_none());
        assert!(PageKey::parse("plain page\n".to_string()).is_none());
        let key = PageKey::parse("LS(1)\nbody\n".to_string()).unwrap();
        assert_eq!(key.file_stem(), "ls.1");
    }

    #[test]
    fn 端点规范化() {
        let table = [
            (
                "HTTPS://API.Example.COM:443/v1/",
                Some("https://api.example.com/v1"),
            ),
            ("http://example.com:80", Some("http://example.com")),
            (
                "http://example.com:8080/v1/",
                Some("http://example.com:8080/v1"),
            ),
            ("https://example.com", Some("https://example.com")),
            ("https://example.com/v1?x=1", None),
            ("https://example.com/v1#frag", None),
            ("ftp://example.com/v1", None),
            ("not a url", None),
        ];
        for (raw, expected) in table {
            assert_eq!(normalize_endpoint(raw).as_deref(), expected, "{raw}");
        }
    }

    #[test]
    fn 注释字段与预转换哈希() {
        let key = PageKey::parse("LS(1)\nbody\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com:443/v1/", "test model");
        let comment = comment_line(&parts, &key).unwrap();
        let fields: Vec<(&str, &str)> = comment[5..comment.len() - 4]
            .split(' ')
            .map(|field| field.split_once('=').unwrap())
            .collect();
        assert_eq!(
            fields,
            vec![
                ("v", "3"),
                ("prompt", "3"),
                ("endpoint", "https://api.example.com/v1"),
                ("model", "test%20model"),
                ("reasoning", "low"),
                ("hash", "7f063821988e288503bb9cc55c4d108d"),
            ],
            "{comment}"
        );
        // 哈希对象是整页预转换文本的 BLAKE3-128。
        assert_eq!(
            hex16(blake3::hash(b"LS(1)\nbody\n").as_bytes()),
            "7f063821988e288503bb9cc55c4d108d"
        );
    }

    #[test]
    fn 落盘命中与键字段逐一比对() {
        let dir = temp("roundtrip");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\nbody\n".to_string()).unwrap();
        let base = parts("zh-CN", "https://api.example.com/v1", "m");
        assert!(cache.get_page(&base, &key).is_none());
        cache.put_page(&base, &key, "译文\n");
        assert_eq!(cache.get_page(&base, &key).unwrap(), "译文\n");

        let path = dir
            .path()
            .join("man-ai/zh-CN")
            .join(format!("{}.md", key.file_stem()));
        let stored = fs::read_to_string(&path).unwrap();
        assert_eq!(
            stored,
            "<!-- v=3 prompt=3 endpoint=https://api.example.com/v1 model=m reasoning=low hash=7f063821988e288503bb9cc55c4d108d -->\n译文\n"
        );

        // 任一键字段变化即不命中。
        for changed in [
            KeyParts {
                endpoint: "https://api.example.com/v2",
                ..base
            },
            KeyParts {
                model: "other",
                ..base
            },
            KeyParts {
                reasoning_effort: "high",
                ..base
            },
        ] {
            assert!(cache.get_page(&changed, &key).is_none(), "{changed:?}");
        }
        // 预转换文本变化（哈希变化）即不命中；页名不同是另一个文件。
        let edited = PageKey::parse("LS(1)\nbody edited\n".to_string()).unwrap();
        assert!(cache.get_page(&base, &edited).is_none());
        let other = PageKey::parse("MV(1)\nbody\n".to_string()).unwrap();
        assert!(cache.get_page(&base, &other).is_none());
        // 语言由目录承载：换语言是另一份条目。
        let ja = parts("ja", "https://api.example.com/v1", "m");
        assert!(cache.get_page(&ja, &key).is_none());
        cache.put_page(&ja, &key, "訳文\n");
        assert_eq!(cache.get_page(&ja, &key).unwrap(), "訳文\n");
        assert!(dir.path().join("man-ai/ja").join("ls.1.md").exists());
    }

    #[test]
    fn 命中不更新_mtime() {
        let dir = temp("mtime");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put_page(&parts, &key, "v\n");
        let path = dir
            .path()
            .join("man-ai/zh-CN")
            .join(format!("{}.md", key.file_stem()));
        let old = SystemTime::now() - Duration::from_secs(86_400);
        set_mtime(&path, old);
        assert_eq!(cache.get_page(&parts, &key).unwrap(), "v\n");
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn 端点无法规范化的页不读不写() {
        let dir = temp("bad-endpoint");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://example.com/v1?x=1", "m");
        assert!(cache.get_page(&parts, &key).is_none());
        cache.put_page(&parts, &key, "v\n");
        assert!(!dir.path().join("man-ai/zh-CN").exists());
    }

    #[test]
    fn 路径布局与权限() {
        let dir = temp("layout");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put_page(&parts, &key, "译文\n");
        let path = dir
            .path()
            .join("man-ai/zh-CN")
            .join(format!("{}.md", key.file_stem()));
        assert_eq!(
            fs::read(&path).unwrap(),
            "<!-- v=3 prompt=3 endpoint=https://api.example.com/v1 model=m reasoning=low hash=1b8056e2b8f4fd9e72a6c1b9b3f1c305 -->\n译文\n"
                .as_bytes()
        );
        let mode = fs::metadata(dir.path().join("man-ai"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::read_dir(dir.path().join("man-ai/tmp")).unwrap().count(),
            0
        );
    }

    #[test]
    fn 读失败按_miss() {
        let dir = temp("miss");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        assert!(cache.get_page(&parts, &key).is_none());
        cache.put_page(&parts, &key, "v\n");
        let path = dir
            .path()
            .join("man-ai/zh-CN")
            .join(format!("{}.md", key.file_stem()));
        fs::remove_file(&path).unwrap();
        assert!(cache.get_page(&parts, &key).is_none());
        // 首行不是合法注释也按 miss。
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not a comment\n译文\n").unwrap();
        assert!(cache.get_page(&parts, &key).is_none());
    }

    #[test]
    fn 关闭时不建目录也不读不写() {
        let dir = temp("disabled");
        let config = Config {
            cache_enabled: false,
            ..Config::default()
        };
        let (cache, warning) = open(&config);
        assert_eq!(warning, None);
        assert!(!cache.enabled());
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put_page(&parts, &key, "v\n");
        assert!(cache.get_page(&parts, &key).is_none());
        assert!(!dir.path().join("man-ai").exists());
    }

    #[test]
    fn 无_home_且无_xdg_降级() {
        let (cache, warning) = open_at(None, Some("   "));
        assert!(!cache.enabled());
        assert_eq!(
            warning.as_deref(),
            Some("cache unavailable (XDG_CACHE_HOME and HOME are unset); continuing without cache")
        );
    }

    #[test]
    fn 只读目录降级() {
        let dir = temp("readonly");
        let base = dir.path().join("cache");
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o500)).unwrap();
        let (cache, warning) = open_at(base.to_str(), None);
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!cache.enabled());
        assert!(warning.unwrap().starts_with("cache unavailable ("));
    }

    #[test]
    fn 并发写同页内容完整() {
        let dir = temp("concurrent");
        let cache = cache_in(&dir);
        let key = PageKey::parse("LS(1)\n".to_string()).unwrap();
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        let values: Vec<String> = (0..8u8)
            .map(|byte| format!("{}\n", "x".repeat(byte as usize + 1)))
            .collect();
        std::thread::scope(|scope| {
            for value in &values {
                scope.spawn(|| cache.put_page(&parts, &key, value));
            }
        });
        let stored = cache.get_page(&parts, &key).unwrap();
        assert!(values.contains(&stored), "{stored}");
    }

    #[test]
    fn 剪枝超限按最旧优先删到八成并清_v1_与陈旧_tmp() {
        let dir = temp("prune");
        let cache = cache_in(&dir);
        let old = SystemTime::now() - Duration::from_secs(86_400);
        let mut paths = Vec::new();
        for (lang, stem) in [
            ("zh-CN", "a.1"),
            ("zh-CN", "b.1"),
            ("ja", "c.1"),
            ("ja", "d.1"),
        ] {
            let path = put_entry(&dir, lang, stem, 400_000);
            set_mtime(&path, old + Duration::from_secs(paths.len() as u64 * 60));
            paths.push(path);
        }
        // 旧哈希布局目录与滞留的 tmp 文件一并清除。
        let v1 = dir.path().join("man-ai/v1/ab/abcd");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("abcd"), b"old").unwrap();
        let stale = dir.path().join("man-ai/tmp/42-0");
        fs::write(&stale, b"stale").unwrap();
        set_mtime(&stale, SystemTime::now() - Duration::from_secs(2 * 3600));

        cache.prune(1);

        let alive: Vec<bool> = paths.iter().map(|path| path.exists()).collect();
        assert_eq!(alive, vec![false, false, true, true]);
        assert!(!dir.path().join("man-ai/v1").exists());
        assert!(!stale.exists());
    }

    #[test]
    fn 零上限只清理不剪枝() {
        let dir = temp("prune-zero");
        let cache = cache_in(&dir);
        let path = put_entry(&dir, "zh-CN", "a.1", 10);
        let v1 = dir.path().join("man-ai/v1");
        fs::create_dir_all(&v1).unwrap();
        cache.prune(0);
        assert!(path.exists());
        assert!(!v1.exists());
    }
}
