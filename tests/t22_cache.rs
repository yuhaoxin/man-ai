//! 块级内容寻址缓存的端到端行为（§9、§13.3）。
//!
//! 缓存根用独立的 `XDG_CACHE_HOME`，断言只关心「请求数是否增加」与条目文件。

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::{
    FakeModel, Reply, TempDir, cache_env, completion, entries, env_refs, error_body, model_config,
    model_config_with, run_pager_tty, run_pipe, text,
};

const LANG: &str = "zh-CN";

#[test]
fn 同输入第二次命中零请求且输出一致() {
    let dir = TempDir::new("t22-hit");
    let model = FakeModel::start(vec![
        Reply::Ok(completion("t1\n", "stop")),
        Reply::Ok(completion("t2\n", "stop")),
    ]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    let input = b"a\n\nb\n";

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], input, &env);
    assert_eq!(model.count(), 2);
    assert_eq!(text(&first.stdout), "t1\n\nt2\n");
    assert_eq!(entries(&dir), 2);

    let second = run_pipe(&dir, &["--translate", "--lang", LANG], input, &env);
    assert_eq!(model.count(), 2, "命中不应再发请求");
    assert_eq!(second.stdout, first.stdout);
    assert_eq!(text(&second.stderr), "");
}

#[test]
fn 关闭缓存时每次都请求且不建目录() {
    let dir = TempDir::new("t22-off");
    let model = FakeModel::start(vec![
        Reply::Ok(completion("t1\n", "stop")),
        Reply::Ok(completion("t1\n", "stop")),
    ]);
    dir.write_config(&model_config(&model, "cache_enabled = false\n"));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    let second = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    assert_eq!(model.count(), 2);
    assert_eq!(second.stdout, first.stdout);
    assert!(!dir.file("cache").exists(), "关闭时不应创建缓存目录");
}

#[test]
fn 失败的回退不入缓存() {
    let dir = TempDir::new("t22-fail");
    let failure = || Reply::Status(500, error_body("server_error", "server_error"));
    let model = FakeModel::start(vec![failure(), failure(), failure(), failure()]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let first = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    assert_eq!(model.count(), 2, "可重试失败重试一次");
    let second = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    assert_eq!(model.count(), 4, "失败的块不能命中");
    assert_eq!(second.stdout, first.stdout);
    assert_eq!(entries(&dir), 0);
}

#[test]
fn 非_utf8_块不发请求也不入缓存() {
    let dir = TempDir::new("t22-binary");
    let model = FakeModel::start(Vec::new());
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"\xff\xfe\n", &env);
    assert_eq!(model.count(), 0);
    assert_eq!(run.stdout, b"\xff\xfe\n");
    assert_eq!(entries(&dir), 0);
}

#[test]
fn 命中不改首屏与顺序() {
    let dir = TempDir::new("t22-order");
    let model = FakeModel::start(vec![
        Reply::Ok(completion("t1\n", "stop")),
        Reply::Ok(completion("t2\n", "stop")),
    ]);
    dir.write_config(&model_config_with(&model, 2, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    let input = b"a\n\nb\n";

    let first = run_pager_tty(&dir, &["--lang", LANG], input, &env);
    assert_eq!(model.count(), 2);
    let second = run_pager_tty(&dir, &["--lang", LANG], input, &env);
    assert_eq!(model.count(), 2, "命中不应再发请求");
    assert_eq!(second.output, first.output);
    assert!(text(&second.output).contains("── man-ai: translation"));
}

#[test]
fn 英文不建目录也不读不写() {
    let dir = TempDir::new("t22-en");
    let model = FakeModel::start(Vec::new());
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);

    let run = run_pipe(&dir, &["--translate", "--lang", "en"], b"a\n", &env);
    assert_eq!(model.count(), 0);
    assert_eq!(run.stdout, b"a\n");
    assert!(!dir.file("cache").exists());
}

#[test]
fn 不可用的缓存告警一次并继续翻译() {
    let dir = TempDir::new("t22-unavailable");
    let model = FakeModel::start(vec![Reply::Ok(completion("t1\n", "stop"))]);
    dir.write_config(&model_config(&model, ""));
    let base = dir.file("cache");
    fs::create_dir(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o500)).unwrap();
    let env = [("XDG_CACHE_HOME", base.to_str().unwrap())];
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &env);
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(model.count(), 1);
    assert_eq!(text(&run.stdout), "t1\n");
    let stderr = text(&run.stderr);
    assert_eq!(stderr.matches("cache unavailable").count(), 1, "{stderr}");
    assert_eq!(run.code, 0);
}
