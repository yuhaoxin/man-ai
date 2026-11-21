//! `-k`/`-f` 用法错误（规格 §3.2、§3.3、§13.3）：stderr 单行、退出 2、不起 man。

mod common;

use std::fs;

use common::{TempDir, recording_man, run_shell_capture, run_shell_tty};

const K_LINE: &str = "man-ai: unsupported option -k; run man -k directly\n";
const F_LINE: &str = "man-ai: unsupported option -f; run man -f directly\n";

/// PATH 指向假 man：man 被执行时会留下 `man-record.txt`。
fn path_with(dir: &TempDir) -> String {
    format!("{}:/usr/bin:/bin", dir.path().display())
}

#[test]
fn 非_tty_k_退出二且不起_man() {
    let dir = TempDir::new("t23-kf-pipe-k");
    recording_man(&dir, 0);
    let run = run_shell_capture(&dir, &["-k", "list"], &[("PATH", &path_with(&dir))]);
    assert_eq!(run.code, 2);
    assert!(
        run.output.is_empty(),
        "{}",
        String::from_utf8_lossy(&run.output)
    );
    assert_eq!(run.stderr, K_LINE);
    assert!(!dir.file("man-record.txt").exists(), "不应执行 man");
}

#[test]
fn 非_tty_f_同理() {
    let dir = TempDir::new("t23-kf-pipe-f");
    recording_man(&dir, 0);
    let run = run_shell_capture(&dir, &["-f", "ls"], &[("PATH", &path_with(&dir))]);
    assert_eq!(run.code, 2);
    assert!(run.output.is_empty());
    assert_eq!(run.stderr, F_LINE);
    assert!(!dir.file("man-record.txt").exists(), "不应执行 man");
}

#[test]
fn 聚类命中按包含的字母报错() {
    let dir = TempDir::new("t23-kf-cluster");
    recording_man(&dir, 0);
    let path = path_with(&dir);
    for (argv, line) in [(vec!["-ak", "list"], K_LINE), (vec!["-af", "ls"], F_LINE)] {
        let run = run_shell_capture(&dir, &argv, &[("PATH", &path)]);
        assert_eq!(run.code, 2, "{argv:?}");
        assert_eq!(run.stderr, line, "{argv:?}");
    }
    assert!(!dir.file("man-record.txt").exists(), "不应执行 man");
}

#[test]
fn tty_行为一致() {
    let dir = TempDir::new("t23-kf-tty");
    recording_man(&dir, 0);
    let run = run_shell_tty(&dir, &["-k", "list"], &[("PATH", &path_with(&dir))]);
    assert_eq!(run.code, 2);
    assert!(run.output.is_empty());
    assert_eq!(run.stderr, K_LINE);
    assert!(!dir.file("man-record.txt").exists(), "不应执行 man");
}

#[test]
fn 翻译开关不影响() {
    let dir = TempDir::new("t23-kf-switch");
    recording_man(&dir, 0);
    let path = path_with(&dir);
    for (argv, line) in [
        (vec!["--translate", "-k", "list"], K_LINE),
        (vec!["--no-translate", "-k", "list"], K_LINE),
        (vec!["--translate", "--lang", "en", "-f", "ls"], F_LINE),
    ] {
        let run = run_shell_capture(&dir, &argv, &[("PATH", &path)]);
        assert_eq!(run.code, 2, "{argv:?}");
        assert_eq!(run.stderr, line, "{argv:?}");
    }
}

#[test]
fn help_与_version_仍优先() {
    let dir = TempDir::new("t23-kf-priority-help");
    let run = run_shell_capture(&dir, &["--help", "-k", "ls"], &[]);
    assert_eq!(run.code, 0);
    assert!(String::from_utf8_lossy(&run.output).starts_with("Usage: man-ai"));
    assert!(run.stderr.is_empty());

    let run = run_shell_capture(&dir, &["--version", "-f"], &[]);
    assert_eq!(run.code, 0);
    assert_eq!(
        run.output,
        format!("man-ai {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(run.stderr.is_empty());
}

#[test]
fn 让路与透传优先于_kf() {
    let dir = TempDir::new("t23-kf-priority-entry");
    recording_man(&dir, 7);
    let path = path_with(&dir);

    let run = run_shell_capture(&dir, &["-k", "-P", "cat"], &[("PATH", &path)]);
    assert_eq!(run.code, 7, "-P 应让路给 man");
    assert!(dir.file("man-record.txt").exists(), "-P 应执行 man");

    fs::remove_file(dir.file("man-record.txt")).unwrap();
    let run = run_shell_capture(&dir, &["-k", "-w", "ls"], &[("PATH", &path)]);
    assert_eq!(run.code, 7, "-w 应透传给 man");
    assert!(dir.file("man-record.txt").exists(), "-w 应执行 man");
    let record = fs::read_to_string(dir.file("man-record.txt")).unwrap();
    assert!(record.contains("args=-k -w ls"), "{record}");
}
