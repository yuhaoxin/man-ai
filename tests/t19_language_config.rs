//! `--lang` 优先级、配置告警与 XDG 路径、薄壳不读配置、英语路径无额外输出。
//!
//! 薄壳模式只在 stdin 是 TTY 时生效，所以那部分要在 pty 下跑；分页器模式
//! 用管道 stdin 即可。

mod common;

use std::fs;

use common::{TempDir, record_value, recording_man, run_pipe, run_shell_tty, text};

/// 薄壳模式下 man 收到的 argv 与 `MANPAGER` 的记录。
struct ShellRun {
    code: i32,
    stderr: String,
    record: std::path::PathBuf,
}

/// 在 pty 下以薄壳模式运行 `man-ai <args>`，PATH 里放假 man。
fn run_shell(dir: &TempDir, args: &[&str], extra_env: &[(&str, &str)]) -> ShellRun {
    let record = recording_man(dir, 0);
    let path = format!("{}:/usr/bin:/bin", dir.path().display());
    let mut env = vec![("PATH", path.as_str())];
    env.extend_from_slice(extra_env);
    let run = run_shell_tty(dir, args, &env);
    ShellRun {
        code: run.code,
        stderr: run.stderr,
        record,
    }
}

// ---------------------------------------------------------------- --lang

#[test]
fn lang_最后一个生效且附加的是规范标签() {
    let dir = TempDir::new("last-wins");
    let run = run_shell(&dir, &["--lang", "zh_CN.UTF-8", "--lang", "pt_br"], &[]);
    assert_eq!(run.code, 0);
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai --lang pt-BR");
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

#[test]
fn lang_非法时退出二且不读配置() {
    let dir = TempDir::new("lang-invalid");
    dir.write_config("colour = \"blue\"\n");
    let out = run_pipe(&dir, &["--lang", "zh CN"], b"page\n", &[]);
    assert_eq!(out.code, 2);
    assert!(out.stdout.is_empty());
    let stderr = text(&out.stderr);
    assert!(stderr.contains("man-ai --help"), "{stderr}");
    assert!(!stderr.contains("config"), "{stderr}");
}

// ---------------------------------------------------------------- 薄壳不读配置

#[test]
fn 薄壳模式不读配置也不附加配置语言() {
    let dir = TempDir::new("shell-config");
    dir.write_config("lang = \"ja\"\ncolour = \"blue\"\n");
    for args in [vec!["ls"], vec!["-P", "less"], vec!["-w", "ls"]] {
        let run = run_shell(&dir, &args, &[]);
        assert_eq!(run.code, 0, "{args:?}");
        let expected_stderr = if args[0] == "-P" {
            "man-ai: -P overrides MANPAGER; not translating\n"
        } else {
            ""
        };
        assert_eq!(run.stderr, expected_stderr, "{args:?}");
        let manpager = record_value(&run.record, "MANPAGER");
        if args[0] == "-P" || args[0] == "-w" {
            assert_eq!(manpager, "<unset>", "{args:?}");
        } else {
            assert_eq!(manpager, "man-ai", "{args:?}");
        }
    }
}

// ---------------------------------------------------------------- 配置告警

#[test]
fn 分页器模式把配置告警写到_stderr_并原文直通() {
    let dir = TempDir::new("config-warn");
    dir.write_config("colour = \"blue\"\nendpoint = \"ftp://example.com\"\n");
    let page: &[u8] = b"LS(1)\n\n     ls - list\n";
    let out = run_pipe(&dir, &["--lang", "zh-CN"], page, &[]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, page);
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr,
        "man-ai: ignoring unknown config key colour\n\
         man-ai: ignoring invalid config key endpoint\n"
    );
}

#[test]
fn 配置文件语法错误时整体按默认并告警() {
    let dir = TempDir::new("config-syntax");
    let path = dir.write_config("lang = \"ja\nendpoint = \"x\"\n");
    let out = run_pipe(&dir, &["--lang", "zh-CN"], b"page\n", &[]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"page\n");
    assert_eq!(
        text(&out.stderr),
        format!(
            "man-ai: failed to parse config ({}); using defaults\n",
            path.display()
        )
    );
}

#[test]
fn 配置语言非法时告警但不退出二() {
    let dir = TempDir::new("config-lang");
    dir.write_config("lang = \"zh CN\"\n");
    let out = run_pipe(&dir, &["--lang", "zh-CN"], b"page\n", &[]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"page\n");
    assert_eq!(
        text(&out.stderr),
        "man-ai: ignoring invalid config key lang: zh CN\n"
    );
}

#[test]
fn 缺配置时静默() {
    let dir = TempDir::new("config-missing");
    let out = run_pipe(&dir, &["--lang", "zh-CN"], b"page\n", &[]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"page\n");
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
}

#[test]
fn xdg_config_home_为空时回退到_home_config() {
    let dir = TempDir::new("config-home");
    let config_dir = dir.path().join(".config").join("man-ai");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), "colour = \"blue\"\n").unwrap();
    let out = run_pipe(
        &dir,
        &["--lang", "zh-CN"],
        b"page\n",
        &[("XDG_CONFIG_HOME", "")],
    );
    assert_eq!(out.code, 0);
    assert_eq!(
        text(&out.stderr),
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
        let out = run_pipe(&dir, &args, page, &env);
        assert_eq!(out.code, 0, "{args:?} {env:?}");
        assert_eq!(out.stdout, page, "{args:?} {env:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} {env:?}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn 配置语言为空时不报错() {
    let dir = TempDir::new("config-lang-empty");
    dir.write_config("lang = \"\"\n");
    let out = run_pipe(&dir, &[], b"page\n", &[]);
    assert_eq!(out.code, 0);
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
}

#[test]
fn 英语路径不输出配置告警_非英语路径输出() {
    let dir = TempDir::new("warnings-lang");
    dir.write_config("colour = \"blue\"\n");
    let page: &[u8] = b"page\n";

    for env in [vec![("LANG", "en_US.UTF-8")], vec![("LANG", "C")]] {
        let out = run_pipe(&dir, &[], page, &env);
        assert_eq!(out.code, 0, "{env:?}");
        assert_eq!(out.stdout, page, "{env:?}");
        assert!(out.stderr.is_empty(), "{env:?}: {}", text(&out.stderr));
    }

    let out = run_pipe(&dir, &[], page, &[("LANG", "zh_CN.UTF-8")]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, page);
    assert_eq!(
        text(&out.stderr),
        "man-ai: ignoring unknown config key colour\n"
    );
}
