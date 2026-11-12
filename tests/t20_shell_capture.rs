//! #20：非 TTY 的 `--translate` 捕获 man 输出后翻译（§5.3）。

mod common;

use common::{
    FakeModel, Reply, TempDir, completion, error_body, fake_man, model_config, run_shell_capture,
    text, write_script,
};

const LANG: &str = "zh-CN";

fn path_with(dir: &TempDir) -> String {
    format!("{}:/usr/bin:/bin", dir.path().display())
}

#[test]
fn 捕获空输出时透传_stderr_与退出码且不翻译() {
    let model = FakeModel::start(vec![]);
    let dir = TempDir::new("capture-empty");
    dir.write_config(&model_config(&model, ""));
    write_script(&dir.file("man"), "#!/bin/sh\necho boom >&2\nexit 5\n");
    let path = path_with(&dir);
    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "ls"],
        &[("PATH", &path)],
    );
    assert_eq!(run.code, 5);
    assert!(run.output.is_empty(), "{:?}", run.output);
    assert_eq!(run.stderr, "boom\n");
    assert_eq!(model.count(), 0);
}

#[test]
fn 捕获后翻译并静默成功且退出码取_man() {
    let page = b"NAME\n\n     ls - list directory contents\n";
    let model = FakeModel::start(vec![
        Reply::Ok(completion("名称\n", "stop")),
        Reply::Ok(completion("     ls - 列出目录内容\n", "stop")),
    ]);
    let dir = TempDir::new("capture-translate");
    dir.write_config(&model_config(&model, ""));
    let record = fake_man(&dir, page, 3);
    let path = path_with(&dir);
    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "ls"],
        &[("PATH", &path)],
    );
    assert_eq!(run.code, 3);
    assert_eq!(text(&run.output), "名称\n\n     ls - 列出目录内容\n");
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    assert_eq!(model.count(), 2);
    // 子进程拿到的是 `MANPAGER=cat`。
    assert_eq!(std::fs::read_to_string(record).unwrap(), "cat");
}

#[test]
fn 全部回退时写_stderr_一行并输出原文() {
    let page = b"NAME\n\n     ls - list\n";
    let model = FakeModel::start(vec![
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
    ]);
    let dir = TempDir::new("capture-fallback");
    dir.write_config(&model_config(&model, ""));
    fake_man(&dir, page, 0);
    let path = path_with(&dir);
    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "ls"],
        &[("PATH", &path)],
    );
    assert_eq!(run.code, 0);
    assert_eq!(run.output, page);
    let stderr = run.stderr;
    assert!(
        stderr.contains("man-ai: backend returned HTTP 500"),
        "{stderr}"
    );
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
    assert_eq!(model.count(), 4);
}

#[test]
fn 没有_translate_时不捕获也不翻译() {
    let model = FakeModel::start(vec![]);
    let dir = TempDir::new("capture-no-flag");
    dir.write_config(&model_config(&model, ""));
    let record = fake_man(&dir, b"page\n", 0);
    let path = path_with(&dir);
    let run = run_shell_capture(&dir, &["--lang", LANG, "ls"], &[("PATH", &path)]);
    assert_eq!(run.code, 0);
    // 未捕获：main 直接 exec man，页面原样到 stdout。
    assert_eq!(run.output, b"page\n");
    assert_eq!(model.count(), 0);
    // 普通路径仍然设置 MANPAGER 为 man-ai（不是 cat）。
    assert_eq!(
        std::fs::read_to_string(record).unwrap(),
        "man-ai --lang zh-CN"
    );
}
