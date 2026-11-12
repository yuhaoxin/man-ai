//! #19 的集成测试：`--lang` 优先级、配置告警与 XDG 路径、薄壳不读配置、
//! 英语路径无额外输出。
//!
//! 薄壳模式只在 stdin 是 TTY 时生效，所以那部分要在 pty 下跑；分页器模式
//! 用管道 stdin 即可。

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_man-ai");

// ---------------------------------------------------------------- 小工具

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "man-ai-t19-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn xdg(&self) -> PathBuf {
        self.0.join("xdg")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// 写 `$XDG_CONFIG_HOME/man-ai/config.toml`，返回路径。
fn write_config(dir: &TempDir, text: &str) -> PathBuf {
    let config_dir = dir.xdg().join("man-ai");
    fs::create_dir_all(&config_dir).unwrap();
    let path = config_dir.join("config.toml");
    fs::write(&path, text).unwrap();
    path
}

/// 非 TTY 运行：stdin/stdout/stderr 都是管道，配置与环境完全隔离。
fn run(args: &[&str], dir: &TempDir, extra_env: &[(&str, &str)], stdin: &[u8]) -> Output {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.xdg())
        .env("TERM", "dumb")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------- 薄壳模式

/// 记录 man 实际收到的 argv 与 `MANPAGER`，然后按给定退出码退出。
fn fake_man(dir: &Path, exit_code: i32) -> PathBuf {
    let record = dir.join("man-record.txt");
    write_script(
        &dir.join("man"),
        &format!(
            "#!/bin/sh\n{{\n\
             echo \"args=$*\"\n\
             echo \"MANPAGER=${{MANPAGER-<unset>}}\"\n\
             }} > '{record}'\nexit {exit_code}\n",
            record = record.display()
        ),
    );
    record
}

fn record_value(record: &Path, key: &str) -> String {
    let text = fs::read_to_string(record).unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or("<missing>")
        .to_string()
}

struct ShellRun {
    code: i32,
    stderr: String,
    record: PathBuf,
}

/// 在 pty 下以薄壳模式运行 `man-ai <args>`，PATH 里放假 man。
fn run_shell(dir: &TempDir, args: &str, extra_env: &[(&str, &str)]) -> ShellRun {
    let record = fake_man(dir.path(), 0);
    let stderr_file = dir.path().join("man-ai-stderr.txt");
    let path = format!("{}:/usr/bin:/bin", dir.path().display());
    let script = format!("'{BIN}' {args} 2>'{err}'\n", err = stderr_file.display());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", &script]);
    command.env_clear();
    command.env("PATH", &path);
    command.env("HOME", dir.path().to_str().unwrap());
    command.env("XDG_CONFIG_HOME", dir.xdg().to_str().unwrap());
    command.env("TERM", "dumb");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    let code = child.wait().unwrap().exit_code() as i32;
    ShellRun {
        code,
        stderr: fs::read_to_string(&stderr_file).unwrap_or_default(),
        record,
    }
}

// ---------------------------------------------------------------- --lang

#[test]
fn lang_最后一个生效且附加的是规范标签() {
    let dir = TempDir::new("last-wins");
    let run = run_shell(&dir, "--lang zh_CN.UTF-8 --lang pt_br", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai --lang pt-BR");
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

#[test]
fn lang_非法时退出二且不读配置() {
    let dir = TempDir::new("lang-invalid");
    write_config(&dir, "colour = \"blue\"\n");
    let out = run(&["--lang", "zh CN"], &dir, &[], b"page\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = stderr_text(&out);
    assert!(stderr.contains("man-ai --help"), "{stderr}");
    assert!(!stderr.contains("config"), "{stderr}");
}

// ---------------------------------------------------------------- 薄壳不读配置

#[test]
fn 薄壳模式不读配置也不附加配置语言() {
    let dir = TempDir::new("shell-config");
    write_config(&dir, "lang = \"ja\"\ncolour = \"blue\"\n");
    for args in ["ls", "-P less", "-w ls"] {
        let run = run_shell(&dir, args, &[]);
        assert_eq!(run.code, 0, "{args}");
        let expected_stderr = if args.starts_with("-P") {
            "man-ai: -P overrides MANPAGER; not translating\n"
        } else {
            ""
        };
        assert_eq!(run.stderr, expected_stderr, "{args}");
        let manpager = record_value(&run.record, "MANPAGER");
        if args.starts_with("-P") || args.starts_with("-w") {
            assert_eq!(manpager, "<unset>", "{args}");
        } else {
            assert_eq!(manpager, "man-ai", "{args}");
        }
    }
}

// ---------------------------------------------------------------- 配置告警

#[test]
fn 分页器模式把配置告警写到_stderr_并原文直通() {
    let dir = TempDir::new("config-warn");
    write_config(
        &dir,
        "colour = \"blue\"\nendpoint = \"ftp://example.com\"\n",
    );
    let page: &[u8] = b"LS(1)\n\n     ls - list\n";
    let out = run(&["--lang", "zh-CN"], &dir, &[], page);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, page);
    let stderr = stderr_text(&out);
    assert_eq!(
        stderr,
        "man-ai: ignoring unknown config key colour\n\
         man-ai: ignoring invalid config key endpoint\n"
    );
}

#[test]
fn 配置文件语法错误时整体按默认并告警() {
    let dir = TempDir::new("config-syntax");
    let path = write_config(&dir, "lang = \"ja\nendpoint = \"x\"\n");
    let out = run(&["--lang", "zh-CN"], &dir, &[], b"page\n");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"page\n");
    assert_eq!(
        stderr_text(&out),
        format!(
            "man-ai: failed to parse config ({}); using defaults\n",
            path.display()
        )
    );
}

#[test]
fn 配置语言非法时告警但不退出二() {
    let dir = TempDir::new("config-lang");
    write_config(&dir, "lang = \"zh CN\"\n");
    let out = run(&["--lang", "zh-CN"], &dir, &[], b"page\n");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"page\n");
    assert_eq!(
        stderr_text(&out),
        "man-ai: ignoring invalid config key lang: zh CN\n"
    );
}

#[test]
fn 缺配置时静默() {
    let dir = TempDir::new("config-missing");
    let out = run(&["--lang", "zh-CN"], &dir, &[], b"page\n");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"page\n");
    assert!(out.stderr.is_empty(), "{}", stderr_text(&out));
}

#[test]
fn xdg_config_home_为空时回退到_home_config() {
    let dir = TempDir::new("config-home");
    let config_dir = dir.path().join(".config").join("man-ai");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), "colour = \"blue\"\n").unwrap();
    let out = run(
        &["--lang", "zh-CN"],
        &dir,
        &[("XDG_CONFIG_HOME", "")],
        b"page\n",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stderr_text(&out),
        "man-ai: ignoring unknown config key colour\n"
    );
}

// ---------------------------------------------------------------- 英语路径

#[test]
fn 英语路径没有额外输出() {
    let dir = TempDir::new("english");
    let page: &[u8] = b"LS(1)\n\n     ls - list directory contents\n";
    for (args, env) in [
        (vec!["--lang", "en"], vec![]),
        (vec!["--lang", "en_US.UTF-8"], vec![]),
        (vec![], vec![("LANG", "en_US.UTF-8")]),
        (vec![], vec![("LANG", "C")]),
        (vec!["--lang", "zh-CN", "--no-translate"], vec![]),
    ] {
        let out = run(&args, &dir, &env, page);
        assert_eq!(out.status.code(), Some(0), "{args:?} {env:?}");
        assert_eq!(out.stdout, page, "{args:?} {env:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} {env:?}: {}",
            stderr_text(&out)
        );
    }
}

#[test]
fn 配置语言为空时不报错() {
    let dir = TempDir::new("config-lang-empty");
    write_config(&dir, "lang = \"\"\n");
    let out = run(&[], &dir, &[], b"page\n");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "{}", stderr_text(&out));
}

#[test]
fn 英语路径不输出配置告警_非英语路径输出() {
    let dir = TempDir::new("warnings-lang");
    write_config(&dir, "colour = \"blue\"\n");
    let page: &[u8] = b"page\n";

    for env in [vec![("LANG", "en_US.UTF-8")], vec![("LANG", "C")]] {
        let out = run(&[], &dir, &env, page);
        assert_eq!(out.status.code(), Some(0), "{env:?}");
        assert_eq!(out.stdout, page, "{env:?}");
        assert!(out.stderr.is_empty(), "{env:?}: {}", stderr_text(&out));
    }

    let out = run(&[], &dir, &[("LANG", "zh_CN.UTF-8")], page);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, page);
    assert_eq!(
        stderr_text(&out),
        "man-ai: ignoring unknown config key colour\n"
    );
}
