//! 块级内容寻址缓存（规格 §9）：键、XDG 布局、原子写、命中与剪枝。
//!
//! 只存采用后的译文；关闭或不可用时整个结构退化为 no-op，调用方无需分支。

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::prompt::PROMPT_VERSION;

/// 回写协议、校验语义或键结构变化时递增（§9.1）。
pub const CACHE_VERSION: u32 = 1;

/// `v1/tmp` 中滞留超过这个时长的文件在剪枝时删除（§9.5）。
const STALE_TMP: Duration = Duration::from_secs(3600);

/// 缓存内容形态；两者互不命中（§9.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// 整页的一个块。
    Page,
    /// `-k` / `-f` 的描述批。
    K,
}

impl Form {
    pub fn tag(self) -> &'static str {
        match self {
            Form::Page => "page",
            Form::K => "k",
        }
    }
}

/// 键里除输入字节外的固定部分；`endpoint` 传配置原值，进键前规范化。
#[derive(Debug, Clone, Copy)]
pub struct KeyParts<'a> {
    pub lang: &'a str,
    pub endpoint: &'a str,
    pub model: &'a str,
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

    /// 命中直接返回文件内容；读失败按 miss。
    pub fn get(&self, form: Form, parts: &KeyParts<'_>, input: &[u8]) -> Option<Vec<u8>> {
        let state = self.state.as_ref()?;
        let key = key(form, parts, input)?;
        fs::read(entry(&state.root, &key)).ok()
    }

    /// 采用之后立即写；失败只记一条告警，不影响翻译。
    pub fn put(&self, form: Form, parts: &KeyParts<'_>, input: &[u8], value: &[u8]) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let Some(key) = key(form, parts, input) else {
            return;
        };
        if write_atomic(&state.root, &entry(&state.root, &key), value).is_err() {
            if let Ok(mut slot) = state.warning.lock() {
                if slot.is_none() {
                    *slot = Some("cache write failed; continuing without cache".to_string());
                }
            }
        }
    }

    /// 取走待输出的告警；管线据此在分页器启动前写 stderr（§5.5）。
    pub fn take_warning(&self) -> Option<String> {
        self.state.as_ref()?.warning.lock().ok()?.take()
    }

    /// 按总大小剪枝到上限的 80%，并清掉陈旧的临时文件（§9.5）。`0` 表示不限。
    pub fn prune(&self, max_mb: u64) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let v1 = state.root.join("v1");
        remove_stale_tmp(&v1.join("tmp"));
        if max_mb == 0 {
            return;
        }
        let limit = max_mb.saturating_mul(1024 * 1024);
        let mut entries = Vec::new();
        let mut total: u64 = 0;
        for shard in fs::read_dir(&v1).into_iter().flatten().flatten() {
            let path = shard.path();
            if path.file_name().is_some_and(|name| name == "tmp") {
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
    let v1 = root.join("v1");
    let tmp = v1.join("tmp");
    fs::create_dir_all(&tmp).map_err(|error| error.to_string())?;
    for dir in [root, &v1, &tmp] {
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

/// `v1/<键前 2 位>/<完整键>`（§9.2）。
fn entry(root: &Path, key: &str) -> PathBuf {
    root.join("v1").join(&key[..2]).join(key)
}

fn write_atomic(root: &Path, target: &Path, value: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{}-", std::process::id()))
        .tempfile_in(root.join("v1").join("tmp"))?;
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

/// 键 = BLAKE3-128，32 位小写十六进制（§9.1）。端点无法规范化时返回 `None`。
pub fn key(form: Form, parts: &KeyParts<'_>, input: &[u8]) -> Option<String> {
    let endpoint = normalize_endpoint(parts.endpoint)?;
    Some(key_versioned(
        CACHE_VERSION,
        PROMPT_VERSION,
        form,
        parts.lang,
        &endpoint,
        parts.model,
        input,
    ))
}

/// 版本号可注入，便于测试旧键不命中。
fn key_versioned(
    cache_version: u32,
    prompt_version: u32,
    form: Form,
    lang: &str,
    endpoint: &str,
    model: &str,
    input: &[u8],
) -> String {
    let cache_version = cache_version.to_string();
    let prompt_version = prompt_version.to_string();
    let mut hasher = blake3::Hasher::new();
    for field in [
        cache_version.as_bytes(),
        prompt_version.as_bytes(),
        form.tag().as_bytes(),
        lang.as_bytes(),
        endpoint.as_bytes(),
        model.as_bytes(),
    ] {
        feed(&mut hasher, field);
    }
    feed(&mut hasher, input);
    hex16(hasher.finalize().as_bytes())
}

/// 每段前置 8 字节小端长度，防止相邻字段拼接出歧义（§9.1）。
fn feed(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&(field.len() as u64).to_le_bytes());
    hasher.update(field);
}

fn hex16(digest: &[u8; 32]) -> String {
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

    #[test]
    fn blake3_128_已知向量() {
        assert_eq!(
            hex16(blake3::hash(b"abc").as_bytes()),
            "6437b3ac38465133ffb63b75273a8db5"
        );
    }

    #[test]
    fn 键是32位小写十六进制() {
        let key = key(
            Form::Page,
            &parts("zh-CN", "https://api.example.com/v1", "m"),
            b"hello\n",
        )
        .unwrap();
        assert_eq!(key.len(), 32);
        assert!(
            key.bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
    }

    #[test]
    fn 长度前缀防止拼接歧义() {
        let left = key(Form::Page, &parts("ab", "https://example.com", "c"), b"").unwrap();
        let right = key(Form::Page, &parts("a", "https://example.com", "bc"), b"").unwrap();
        assert_ne!(left, right);
    }

    #[test]
    fn 任一段变化键就变化() {
        let base = key(
            Form::Page,
            &parts("zh-CN", "https://api.example.com/v1", "m"),
            b"input\n",
        )
        .unwrap();
        let changed = [
            key(
                Form::Page,
                &parts("ja", "https://api.example.com/v1", "m"),
                b"input\n",
            ),
            key(
                Form::Page,
                &parts("zh-CN", "https://api.example.com/v2", "m"),
                b"input\n",
            ),
            key(
                Form::Page,
                &parts("zh-CN", "https://api.example.com/v1", "other"),
                b"input\n",
            ),
            key(
                Form::Page,
                &parts("zh-CN", "https://api.example.com/v1", "m"),
                b"input!\n",
            ),
        ];
        for other in changed {
            assert_ne!(base, other.unwrap());
        }
    }

    #[test]
    fn 形态互不命中() {
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        assert_ne!(
            key(Form::Page, &parts, b"x").unwrap(),
            key(Form::K, &parts, b"x").unwrap()
        );
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
    fn 版本变化后键不同() {
        let left = key_versioned(1, 2, Form::Page, "zh-CN", "https://e.com", "m", b"x");
        let cache_bump = key_versioned(2, 2, Form::Page, "zh-CN", "https://e.com", "m", b"x");
        let prompt_bump = key_versioned(1, 3, Form::Page, "zh-CN", "https://e.com", "m", b"x");
        assert_ne!(left, cache_bump);
        assert_ne!(left, prompt_bump);
        assert_ne!(cache_bump, prompt_bump);
    }

    #[test]
    fn 路径布局与权限() {
        let dir = temp("layout");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put(Form::Page, &parts, b"block\n", "译文\n".as_bytes());
        let key = key(Form::Page, &parts, b"block\n").unwrap();
        let path = dir
            .path()
            .join("man-ai")
            .join("v1")
            .join(&key[..2])
            .join(&key);
        assert_eq!(fs::read(&path).unwrap(), b"\xe8\xaf\x91\xe6\x96\x87\n");
        for dir_path in [
            path.parent().unwrap().to_path_buf(),
            dir.path().join("man-ai"),
        ] {
            let mode = fs::metadata(&dir_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir_path.display());
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            fs::read_dir(dir.path().join("man-ai/v1/tmp"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            cache.get(Form::Page, &parts, b"block\n").unwrap(),
            b"\xe8\xaf\x91\xe6\x96\x87\n"
        );
    }

    #[test]
    fn 命中不更新_mtime() {
        let dir = temp("mtime");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put(Form::Page, &parts, b"block\n", b"v");
        let key = key(Form::Page, &parts, b"block\n").unwrap();
        let path = entry(&dir.path().join("man-ai"), &key);
        let old = SystemTime::now() - Duration::from_secs(86_400);
        set_mtime(&path, old);
        assert_eq!(cache.get(Form::Page, &parts, b"block\n").unwrap(), b"v");
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn 读失败按_miss() {
        let dir = temp("miss");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        assert!(cache.get(Form::Page, &parts, b"block\n").is_none());
        cache.put(Form::Page, &parts, b"block\n", b"v");
        let key = key(Form::Page, &parts, b"block\n").unwrap();
        fs::remove_file(entry(&dir.path().join("man-ai"), &key)).unwrap();
        assert!(cache.get(Form::Page, &parts, b"block\n").is_none());
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
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put(Form::Page, &parts, b"block\n", b"v");
        assert!(cache.get(Form::Page, &parts, b"block\n").is_none());
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
    fn 并发写同键内容完整() {
        let dir = temp("concurrent");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        let values: Vec<Vec<u8>> = (0..8u8).map(|byte| vec![byte; 64 * 1024]).collect();
        std::thread::scope(|scope| {
            for value in &values {
                scope.spawn(|| cache.put(Form::Page, &parts, b"block\n", value));
            }
        });
        let stored = cache.get(Form::Page, &parts, b"block\n").unwrap();
        assert!(values.contains(&stored));
    }

    #[test]
    fn 剪枝超限按最旧优先删到八成() {
        let dir = temp("prune");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        let keys: Vec<String> = (0..4)
            .map(|index| {
                let input = format!("block{index}\n");
                cache.put(Form::Page, &parts, input.as_bytes(), &vec![b'x'; 400_000]);
                key(Form::Page, &parts, input.as_bytes()).unwrap()
            })
            .collect();
        let root = dir.path().join("man-ai");
        let old = SystemTime::now() - Duration::from_secs(86_400);
        for (index, key) in keys.iter().enumerate() {
            let path = entry(&root, key);
            set_mtime(&path, old + Duration::from_secs(index as u64 * 60));
        }
        cache.prune(1);
        let alive: Vec<bool> = keys.iter().map(|key| entry(&root, key).exists()).collect();
        assert_eq!(alive, vec![false, false, true, true]);
    }

    #[test]
    fn 剪枝零表示不限() {
        let dir = temp("nolimit");
        let cache = cache_in(&dir);
        let parts = parts("zh-CN", "https://api.example.com/v1", "m");
        cache.put(Form::Page, &parts, b"a\n", &vec![b'x'; 1_500_000]);
        cache.prune(0);
        assert!(cache.get(Form::Page, &parts, b"a\n").is_some());
    }

    #[test]
    fn 剪枝删除超过一小时的临时文件() {
        let dir = temp("stale");
        let cache = cache_in(&dir);
        let tmp = dir.path().join("man-ai/v1/tmp");
        let stale = tmp.join("1-old");
        let fresh = tmp.join("1-new");
        fs::write(&stale, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        set_mtime(&stale, SystemTime::now() - Duration::from_secs(7200));
        cache.prune(0);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }
}
