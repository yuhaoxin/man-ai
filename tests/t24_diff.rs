//! 与平台 man 的差分验收（§13.2）。
//!
//! 只覆盖不翻译路径：`--no-translate`、`--lang en`、`LC_ALL=C`、`LC_ALL=POSIX`、非 TTY 默认路径。
//! 基线在运行时由本机 man 现场生成，不入库；唯一归一化是 `-w` 输出的绝对 man 路径。

mod common;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use common::{BIN, RealMan, assert_bytes_eq};

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 只归一化绝对 man 路径：`-w` 的输出是本机 MANPATH 下的真实路径。
///
/// 按字节替换，不经过 UTF-8 解码。
fn normalize_path(bytes: &[u8], manpath: &Path) -> Vec<u8> {
    let needle = manpath.as_os_str().as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(at) = rest
        .windows(needle.len())
        .position(|window| window == needle)
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(b"<MANPATH>");
        rest = &rest[at + needle.len()..];
    }
    out.extend_from_slice(rest);
    out
}

#[test]
fn 不翻译路径与平台_man_字节一致() {
    let Some(real) = RealMan::new("t24-diff") else {
        eprintln!("跳过：本机没有 man");
        return;
    };
    let page = "1 man-ai-fixture";

    let (baseline, baseline_stderr, baseline_code) = real.run(
        "baseline",
        &real.man_command(page),
        &[("MANPAGER", "cat")],
        "C.UTF-8",
    );
    assert_eq!(baseline_code, 0, "基线 man 失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "基线 man 写了 stderr");
    assert!(!baseline.is_empty(), "基线没有输出，fixture 页可能没找到");

    // 四条不翻译路径；各自与同一次基线比。
    for (label, manpager, lc_all) in [
        ("--no-translate", "man-ai --no-translate", "C.UTF-8"),
        ("--lang en", "man-ai --lang en", "C.UTF-8"),
        ("LC_ALL=C", "man-ai", "C"),
        ("LC_ALL=POSIX", "man-ai", "POSIX"),
    ] {
        let (ours, ours_stderr, ours_code) = real.run(
            &format!("ours-{}", label.replace([' ', '='], "-")),
            &real.man_command(page),
            &[("MANPAGER", manpager), ("MAN_AI_PAGER", "cat")],
            lc_all,
        );
        assert_eq!(ours_stderr, "", "{label} man-ai 路径写了 stderr");
        assert_eq!(ours_code, baseline_code, "{label} 退出码应与 man 一致");
        assert_bytes_eq(&ours, &baseline, label);
    }
}

#[test]
fn 透传入口_w_的输出与平台_man_字节一致() {
    let Some(real) = RealMan::new("t24-diff-w") else {
        eprintln!("跳过：本机没有 man");
        return;
    };

    let (baseline, baseline_stderr, baseline_code) = real.run(
        "w-baseline",
        &real.man_command("-w man-ai-fixture"),
        &[],
        "C.UTF-8",
    );
    assert_eq!(baseline_code, 0, "-w 基线失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "-w 基线写了 stderr");
    assert!(!baseline.is_empty(), "-w 基线没有输出");

    // 薄壳模式：stdin 是 pty，`-w` 直接透传给 man。
    let (ours, ours_stderr, ours_code) = real.run(
        "w-ours",
        &format!("{} -w man-ai-fixture", shell_quote_str(BIN)),
        &[],
        "C.UTF-8",
    );
    assert_eq!(ours_stderr, "", "-w man-ai 写了 stderr");
    assert_eq!(ours_code, baseline_code, "-w 退出码应与 man 一致");

    let manpath = real.manpath().to_path_buf();
    assert_bytes_eq(
        &normalize_path(&ours, &manpath),
        &normalize_path(&baseline, &manpath),
        "-w（只归一化绝对 man 路径）",
    );
}

/// 非 TTY 默认路径：`man 页 | man-ai` 与 `man 页` 的输出逐字节一致。
#[test]
fn 非_tty_默认路径与平台_man_字节一致() {
    let Some(real) = RealMan::new("t24-diff-pipe") else {
        eprintln!("跳过：本机没有 man");
        return;
    };
    let stderr_file = real.dir.file("pipe.stderr");

    let run = |command: &str| {
        let output = Command::new("/bin/sh")
            .args(["-c", command])
            .env_clear()
            .env("PATH", &real.base_path)
            .env("HOME", real.dir.path())
            .env("MANPATH", &real.manpath)
            .env("MANWIDTH", "80")
            .env("TERM", "dumb")
            .env("MANPAGER", "cat")
            .env("MAN_AI_PAGER", "cat")
            .stderr(fs::File::create(&stderr_file).unwrap())
            .output()
            .unwrap();
        (
            output.stdout,
            fs::read_to_string(&stderr_file).unwrap_or_default(),
            output.status.code(),
        )
    };

    let (baseline, baseline_stderr, baseline_code) = run(&real.man_command("1 man-ai-fixture"));
    assert_eq!(baseline_code, Some(0), "基线 man 失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "基线 man 写了 stderr");
    assert!(!baseline.is_empty(), "基线没有输出");

    let (ours, ours_stderr, ours_code) = run(&format!(
        "{} | {}",
        real.man_command("1 man-ai-fixture"),
        shell_quote_str(BIN)
    ));
    assert_eq!(ours_stderr, "", "非 TTY man-ai 路径写了 stderr");
    assert_eq!(ours_code, baseline_code, "退出码应与 man 一致");
    assert_bytes_eq(&ours, &baseline, "非 TTY 默认路径");
}
