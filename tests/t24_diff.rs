//! #24：与平台 man 的差分验收（§13.2）。
//!
//! 只覆盖不翻译路径：`--no-translate`、`--lang en`、`LC_ALL=C`、`LC_ALL=POSIX`、非 TTY 默认路径。
//! 基线在运行时由本机 man 现场生成，不入库；唯一归一化是 `-w` 输出的绝对 man 路径。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{BIN, TempDir, run_pty_command, write_script};

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 仓库内自写的 roff fixture 页，写进临时 MANPATH。
fn fixture_manpath(root: &Path) -> PathBuf {
    let man1 = root.join("man/man1");
    fs::create_dir_all(&man1).unwrap();
    fs::write(
        man1.join("man-ai-fixture.1"),
        ".TH MAN-AI-FIXTURE 1 \"2026-01-01\" \"man-ai 0.1.0\" \"General Commands Manual\"\n\
         .SH NAME\n\
         man-ai-fixture \\- fixture page for differential tests\n\
         .SH DESCRIPTION\n\
         .PP\n\
         This page exists only to pin byte compatibility with the platform man.\n\
         .PP\n\
         A second paragraph keeps the page longer than one block.\n\
         .SH OPTIONS\n\
         .TP\n\
         .B \\-\\-no-translate\n\
         Pass the rendered page through unchanged.\n",
    )
    .unwrap();
    root.join("man")
}

/// 真 man + 固定 MANPATH 的差分环境；基线每次现场生成。
struct RealMan {
    dir: TempDir,
    man: PathBuf,
    base_path: String,
    manpath: PathBuf,
}

impl RealMan {
    fn new(tag: &str) -> Option<Self> {
        let man = find_in_path("man")?;
        let dir = TempDir::new(tag);
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        // MANPAGER 里的 man-ai 从 PATH 解析（§4.2）。
        write_script(
            &bin_dir.join("man-ai"),
            &format!("#!/bin/sh\nexec '{BIN}' \"$@\"\n"),
        );
        let manpath = fixture_manpath(dir.path());
        Some(RealMan {
            dir,
            base_path: format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
            man,
            manpath,
        })
    }

    /// pty 下运行，stderr 落到单独文件。
    fn run_pty(
        &self,
        tag: &str,
        command: &str,
        extra_env: &[(&str, &str)],
        lc_all: &str,
    ) -> (Vec<u8>, String, i32) {
        let stderr_file = self.dir.file(&format!("{tag}.stderr"));
        let script = format!("{command} 2>{}\n", shell_quote(&stderr_file));
        let mut env = vec![
            ("PATH", self.base_path.as_str()),
            ("HOME", self.dir.path().to_str().unwrap()),
            ("MANPATH", self.manpath.to_str().unwrap()),
            ("MANWIDTH", "80"),
            ("LC_ALL", lc_all),
            ("TERM", "dumb"),
        ];
        env.extend_from_slice(extra_env);
        let (output, code) = run_pty_command(&self.dir, &script, &env);
        let stderr = fs::read_to_string(&stderr_file).unwrap_or_default();
        (output, stderr, code)
    }

    fn man_command(&self, args: &str) -> String {
        format!("{} {args}", shell_quote(&self.man))
    }
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(path.to_str().unwrap())
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 只归一化绝对 man 路径：`-w` 的输出是本机 MANPATH 下的真实路径。
fn normalize_path(bytes: &[u8], manpath: &Path) -> Vec<u8> {
    let needle = manpath.to_string_lossy();
    let text = String::from_utf8_lossy(bytes);
    text.replace(needle.as_ref(), "<MANPATH>").into_bytes()
}

fn assert_same(ours: &[u8], baseline: &[u8], label: &str) {
    assert_eq!(
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(baseline),
        "{label}：stdout 应与平台 man 逐字节一致"
    );
}

#[test]
fn 不翻译路径与平台_man_字节一致() {
    let Some(real) = RealMan::new("t24-diff") else {
        eprintln!("跳过：本机没有 man");
        return;
    };
    let page = "1 man-ai-fixture";

    let (baseline, baseline_stderr, baseline_code) = real.run_pty(
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
        let (ours, ours_stderr, ours_code) = real.run_pty(
            &format!("ours-{}", label.replace([' ', '='], "-")),
            &real.man_command(page),
            &[("MANPAGER", manpager), ("MAN_AI_PAGER", "cat")],
            lc_all,
        );
        assert_eq!(ours_stderr, "", "{label} man-ai 路径写了 stderr");
        assert_eq!(ours_code, baseline_code, "{label} 退出码应与 man 一致");
        assert_same(&ours, &baseline, label);
    }
}

#[test]
fn 透传入口_w_的输出与平台_man_字节一致() {
    let Some(real) = RealMan::new("t24-diff-w") else {
        eprintln!("跳过：本机没有 man");
        return;
    };

    let (baseline, baseline_stderr, baseline_code) = real.run_pty(
        "w-baseline",
        &real.man_command("-w man-ai-fixture"),
        &[],
        "C.UTF-8",
    );
    assert_eq!(baseline_code, 0, "-w 基线失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "-w 基线写了 stderr");
    assert!(!baseline.is_empty(), "-w 基线没有输出");

    // 薄壳模式：stdin 是 pty，`-w` 直接透传给 man。
    let (ours, ours_stderr, ours_code) = real.run_pty(
        "w-ours",
        &format!("{} -w man-ai-fixture", shell_quote_str(BIN)),
        &[],
        "C.UTF-8",
    );
    assert_eq!(ours_stderr, "", "-w man-ai 写了 stderr");
    assert_eq!(ours_code, baseline_code, "-w 退出码应与 man 一致");

    let manpath = real.manpath.clone();
    assert_eq!(
        String::from_utf8_lossy(&normalize_path(&ours, &manpath)),
        String::from_utf8_lossy(&normalize_path(&baseline, &manpath)),
        "-w stdout 只允许绝对 man 路径不同"
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
    assert_same(&ours, &baseline, "非 TTY 默认路径");
}
