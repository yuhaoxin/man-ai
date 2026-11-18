//! 整页文件缓存的端到端行为（§9、§13.3）。
//!
//! 缓存根用独立的 `XDG_CACHE_HOME`，断言只关心请求数与缓存条目文件。

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{
    FakeModel, Reply, TempDir, cache_env, completion, env_refs, error_body, model_config,
    model_config_with, run_pipe, text,
};

const LANG: &str = "zh-CN";
/// 首行页眉 `LS(1)` + 正文，两块。
const INPUT: &[u8] = b"LS(1)\n\nlist directory contents\n";

fn entry(dir: &TempDir) -> PathBuf {
    dir.file("cache/man-ai/zh-CN/ls.1.md")
}

/// 缓存条目首行注释的键字段，按 `k=v` 逐一取出。
fn comment_fields(path: &Path) -> Vec<(String, String)> {
    let first = fs::read_to_string(path)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let first = first
        .strip_prefix("<!-- ")
        .and_then(|rest| rest.strip_suffix(" -->"))
        .unwrap();
    first
        .split(' ')
        .map(|field| {
            let (key, value) = field.split_once('=').unwrap();
            (key.to_string(), value.to_string())
        })
        .collect()
}

/// 预转换全文的 BLAKE3-128（§9.1 哈希对象）。
fn blake3_128(bytes: &[u8]) -> String {
    blake3::hash(bytes).as_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn headered_model() -> FakeModel {
    FakeModel::start_by_content(vec![
        (
            "LS(1)\n".to_string(),
            Reply::Ok(completion("页眉译文\n", "stop")),
        ),
        (
            "list directory contents\n".to_string(),
            Reply::Ok(completion("正文译文\n", "stop")),
        ),
    ])
}

#[test]
fn 同页第二次运行零请求且输出一致() {
    let dir = TempDir::new("t22-hit");
    let model = headered_model();
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 2);
    assert_eq!(text(&first.stdout), "页眉译文\n\n正文译文\n");
    assert_eq!(first.code, 0);
    assert!(entry(&dir).exists());

    let second = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 2, "命中不应再发请求");
    assert_eq!(second.stdout, first.stdout);
    assert_eq!(text(&second.stderr), "");
}

#[test]
fn retranslate_触发请求并覆盖旧条目() {
    let dir = TempDir::new("t22-retranslate");
    let model = headered_model();
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 2);

    // 篡改条目：只有真正的覆盖写才能恢复可命中状态。
    fs::write(entry(&dir), "garbage\n").unwrap();
    let second = run_pipe(
        &dir,
        &["--translate", "--retranslate", "--lang", LANG],
        INPUT,
        &env,
    );
    assert_eq!(model.count(), 4, "--retranslate 跳过缓存读取");
    assert_eq!(second.stdout, first.stdout);

    let third = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 4, "覆盖写的新条目可命中");
    assert_eq!(third.stdout, first.stdout);
}

#[test]
fn 缓存条目首行注释键字段逐一比对() {
    let dir = TempDir::new("t22-fields");
    let model = headered_model();
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);

    // 配置端点带尾斜杠，注释里是规范化后的值。
    assert_eq!(
        comment_fields(&entry(&dir)),
        vec![
            ("v".to_string(), "3".to_string()),
            ("prompt".to_string(), "4".to_string()),
            ("endpoint".to_string(), model.endpoint().to_string()),
            ("model".to_string(), "test-model".to_string()),
            ("reasoning".to_string(), "low".to_string()),
            ("hash".to_string(), blake3_128(INPUT)),
        ]
    );
    // 条目 = 注释行 + 整页译文，命中输出前剥除注释行。
    let stored = fs::read_to_string(entry(&dir)).unwrap();
    assert!(stored.ends_with("\n页眉译文\n\n正文译文\n"), "{stored}");
}

#[test]
fn 含回退块的页不写缓存() {
    let dir = TempDir::new("t22-fallback");
    let failure = || Reply::Status(500, error_body("server_error", "server_error"));
    let model = FakeModel::start(vec![
        Reply::Ok(completion("页眉译文\n", "stop")),
        failure(),
        failure(),
    ]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let run = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 3, "可重试失败重试一次");
    assert_eq!(text(&run.stdout), "页眉译文\n\nlist directory contents\n");
    assert!(!entry(&dir).exists(), "含回退块的页不落盘");
}

#[test]
fn 页眉解析失败的页不缓存() {
    let dir = TempDir::new("t22-no-header");
    let model = FakeModel::start_by_content(vec![(
        "a\n".to_string(),
        Reply::Ok(completion("t\n", "stop")),
    )]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    assert_eq!(model.count(), 1);
    assert_eq!(text(&run.stdout), "t\n");
    assert!(
        !dir.file("cache/man-ai/zh-CN").exists(),
        "无页眉的页不建语言目录"
    );
}

#[test]
fn 命中零请求且阅读器输出一致() {
    let dir = TempDir::new("t22-order");
    let model = headered_model();
    dir.write_config(&model_config_with(&model, 4, ""));
    let env = cache_env(&dir);

    let run = |dir: &TempDir| {
        common::run_reader_tty(
            dir,
            &["--lang", LANG],
            INPUT,
            &env,
            "── man-ai: translated (Simplified Chinese) ──",
            &["q"],
        )
    };
    let first = run(&dir);
    assert_eq!(model.count(), 2);
    let second = run(&dir);
    assert_eq!(model.count(), 2, "命中不应再发请求");
    // 阅读器输出含页首行与尾行；elapsed 各不相同，不比较时间行。
    // ratatui 的帧 diff 跳过空格，断言前去掉空格。
    for text in [first.text(), second.text()] {
        let compact = common::compact(&text);
        assert!(
            compact.contains("──man-ai:translation(SimplifiedChinese)──"),
            "{compact}"
        );
        assert!(
            compact.contains("──man-ai:translated(SimplifiedChinese)──"),
            "{compact}"
        );
        assert!(compact.contains("页眉译文"), "{compact}");
        assert!(compact.contains("正文译文"), "{compact}");
    }
    // 第二次是整页缓存命中：零请求，状态行只有 elapsed 与 cache hit（§8.3）。
    let hit = common::compact(&second.text());
    // 帧流里最后的指标行是 cache hit（§8.3）：帧 diff 只重写变化格，
    // 旧状态行 `tps` 的 `t` 与新 `hit` 的 `t` 同位，流里是 `cachehi`；
    // 完整形态由阅读器单元测试断言。
    assert!(hit.contains("cachehi"), "{hit}");
    assert_eq!(first.code, 0);
    assert_eq!(second.code, 0);
}

#[test]
fn 关闭缓存时每次都请求且不建目录() {
    let dir = TempDir::new("t22-off");
    let model = headered_model();
    dir.write_config(&model_config(&model, "cache_enabled = false\n"));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    let second = run_pipe(&dir, &["--translate", "--lang", LANG], INPUT, &env);
    assert_eq!(model.count(), 4);
    assert_eq!(second.stdout, first.stdout);
    assert!(!dir.file("cache").exists(), "关闭时不应创建缓存目录");
}

#[test]
fn 英文不建目录也不读不写() {
    let dir = TempDir::new("t22-en");
    let model = FakeModel::start(Vec::new());
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let run = run_pipe(&dir, &["--translate", "--lang", "en"], b"LS(1)\n", &env);
    assert_eq!(model.count(), 0);
    assert_eq!(run.stdout, b"LS(1)\n");
    assert!(!dir.file("cache").exists());
}

#[test]
fn 进入翻译路径删除旧_v1_目录() {
    let dir = TempDir::new("t22-v1");
    let model = FakeModel::start_by_content(vec![(
        "a\n".to_string(),
        Reply::Ok(completion("t\n", "stop")),
    )]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    let v1 = dir.file("cache/man-ai/v1/ab");
    fs::create_dir_all(&v1).unwrap();
    fs::write(v1.join("abcd"), b"old layout").unwrap();

    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    assert_eq!(run.code, 0);
    assert!(!dir.file("cache/man-ai/v1").exists(), "旧布局直接删除");
}

#[test]
fn 不可用的缓存告警一次并继续翻译() {
    let dir = TempDir::new("t22-unavailable");
    let model = headered_model();
    dir.write_config(&model_config(&model, ""));
    let base = dir.file("cache");
    fs::create_dir(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o500)).unwrap();
    let env = [("XDG_CACHE_HOME", base.to_str().unwrap())];
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"LS(1)\n", &env);
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(model.count(), 1);
    assert_eq!(text(&run.stdout), "页眉译文\n");
    let stderr = text(&run.stderr);
    assert_eq!(stderr.matches("cache unavailable").count(), 1, "{stderr}");
    assert_eq!(run.code, 0);
}
