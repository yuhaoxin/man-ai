//! CLI、薄壳模式、分页器模式与真 man 差分的集成测试。
//!
//! 薄壳模式的判定依赖 stdin 是 TTY，分页器链依赖 stdout 是 TTY，两者都只能
//! 在 pty 下观察；pty 里的 `sh -c` 让需要方向的一端接管道。

mod common;

use std::fs;

use common::{
    BIN, RealMan, TempDir, assert_bytes_eq, record_value, recorded_args, recording_man, run_pipe,
    run_pty_command, write_script,
};

// ---------------------------------------------------------------- 小工具

/// pty 会把 `\n` 写成 `\r\n`，比较文本前统一掉。
fn pty_text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).replace("\r\n", "\n")
}

struct PtyOut {
    output: Vec<u8>,
    code: i32,
}

// ---------------------------------------------------------------- 假 man / 假分页器

/// 假内层分页器：记录 argv，原样输出 stdin，按给定退出码退出。
fn fake_pager(dir: &TempDir, name: &str, exit_code: i32) -> std::path::PathBuf {
    let argv_file = dir.file(&format!("{name}-argv.txt"));
    let script = dir.file(name);
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{argv}'\ncat\nexit {exit_code}\n",
            argv = argv_file.display()
        ),
    );
    script
}

struct ShellRun {
    pty: Vec<u8>,
    code: i32,
    stderr: String,
    record: std::path::PathBuf,
}

/// 在 pty 下以薄壳模式运行 `man-ai <args>`；`args` 是 shell 片段，PATH 里放假 man。
fn run_shell(dir: &TempDir, args: &str, extra_env: &[(&str, &str)], man_exit: i32) -> ShellRun {
    let record = recording_man(dir, man_exit);
    let stderr_file = dir.file("man-ai-stderr.txt");
    let path = format!("{}:/usr/bin:/bin", dir.path().display());
    let script = format!("'{BIN}' {args} 2>'{err}'\n", err = stderr_file.display());
    let mut env = vec![
        ("PATH", path.as_str()),
        ("HOME", dir.path().to_str().unwrap()),
        ("TERM", "dumb"),
    ];
    env.extend_from_slice(extra_env);
    let (output, code) = run_pty_command(dir, &script, &env);
    ShellRun {
        pty: output,
        code,
        stderr: fs::read_to_string(&stderr_file).unwrap_or_default(),
        record,
    }
}

// ---------------------------------------------------------------- --help / --version

#[test]
fn help_打印用法与五个自有参数() {
    let dir = TempDir::new("t18-help");
    let out = run_pipe(&dir, &["--help"], b"", &[]);
    assert_eq!(out.code, 0);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Usage: man-ai"), "{text}");
    for own in [
        "--help",
        "--version",
        "--lang",
        "--translate",
        "--no-translate",
    ] {
        assert!(text.contains(own), "帮助里缺少 {own}");
    }
    for key in ["MANPAGER", "MAN_AI_PAGER"] {
        assert!(text.contains(key), "帮助里缺少 {key}");
    }
    assert!(
        text.trim_end()
            .ends_with("All other arguments are passed to man unchanged."),
        "{text}"
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn version_打印名字与语义版本() {
    let dir = TempDir::new("t18-version");
    let out = run_pipe(&dir, &["--version"], b"", &[]);
    assert_eq!(out.code, 0);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("man-ai {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn help_与_version_优先于语言校验与入口判定() {
    let dir = TempDir::new("t18-help-priority");
    for argv in [
        vec!["--lang", "非法 值", "--help"],
        vec!["-P", "less", "--version"],
        vec!["--lang", "--help"],
    ] {
        let out = run_pipe(&dir, &argv, b"", &[]);
        assert_eq!(out.code, 0, "{argv:?} 应成功");
        assert!(!out.stdout.is_empty());
    }
}

#[test]
fn lang_缺值空值形状非法退出二() {
    let dir = TempDir::new("t18-lang-invalid");
    for argv in [
        vec!["--lang"],
        vec!["--lang="],
        vec!["--lang", ""],
        vec!["--lang", "   "],
        vec!["--lang", "zh CN"],
        vec!["--lang", "中文"],
        vec!["--lang", "-zh"],
        vec!["ls", "--lang"],
    ] {
        let out = run_pipe(&dir, &argv, b"", &[]);
        assert_eq!(out.code, 2, "{argv:?} 应退出 2");
        assert!(out.stdout.is_empty(), "{argv:?} 不应写 stdout");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("man-ai --help"),
            "{argv:?} 报错应指向 --help：{stderr}"
        );
    }
}

#[test]
fn lang_合法时不报错并进入分页器模式() {
    // stdout 不是 TTY → 分页器模式，原文直通 stdout。
    let dir = TempDir::new("t18-lang-ok");
    let out = run_pipe(
        &dir,
        &["--lang", "zh_CN.UTF-8", "--no-translate"],
        b"page bytes\n",
        &[],
    );
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"page bytes\n");
    assert!(out.stderr.is_empty());
}

// ---------------------------------------------------------------- 分页器模式

#[test]
fn 分页器模式读完_stdin_并直写_stdout() {
    let dir = TempDir::new("t18-pager");
    let input: &[u8] = b"LS(1)\n\n     l\x1bls\x08s - list\n\n\nend without newline";
    let out = run_pipe(&dir, &[], input, &[]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, input);
    assert!(out.stderr.is_empty());
}

#[test]
fn 分页器模式忽略_stdin_为空的情形() {
    let dir = TempDir::new("t18-pager-empty");
    let out = run_pipe(&dir, &[], b"", &[]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------- 薄壳模式：MANPAGER

#[test]
fn 薄壳保存原_manpager_并设为_man_ai() {
    let dir = TempDir::new("shell-save");
    let run = run_shell(&dir, "ls", &[("MANPAGER", "less -sR")], 0);
    assert_eq!(run.code, 0);
    assert_eq!(recorded_args(&run.record), "ls");
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai");
    assert_eq!(record_value(&run.record, "MAN_AI_PAGER"), "less -sR");
    assert_eq!(
        record_value(&run.record, "MAN_KEEP_FORMATTING"),
        "<unset>",
        "不得设置 MAN_KEEP_FORMATTING"
    );
    assert!(run.stderr.is_empty());
    assert!(run.pty.is_empty(), "薄壳模式不应自己写 stdout");
}

#[test]
fn 薄壳原_manpager_为空时不保存() {
    let dir = TempDir::new("shell-empty");
    let run = run_shell(&dir, "ls", &[("MANPAGER", "")], 0);
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai");
    assert_eq!(record_value(&run.record, "MAN_AI_PAGER"), "<unset>");
}

#[test]
fn 薄壳把显式自有参数附加到_manpager() {
    let dir = TempDir::new("shell-attach");
    let run = run_shell(&dir, "--lang zh_cn --translate ls", &[], 0);
    assert_eq!(recorded_args(&run.record), "ls");
    assert_eq!(
        record_value(&run.record, "MANPAGER"),
        "man-ai --lang zh-CN --translate"
    );

    let run = run_shell(&dir, "--no-translate --lang ja ls", &[], 0);
    assert_eq!(
        record_value(&run.record, "MANPAGER"),
        "man-ai --lang ja --no-translate"
    );
}

#[test]
fn 薄壳翻译开关最右生效并附加() {
    let dir = TempDir::new("shell-rightmost");
    let run = run_shell(&dir, "--translate --no-translate ls", &[], 0);
    assert_eq!(
        record_value(&run.record, "MANPAGER"),
        "man-ai --no-translate"
    );
}

#[test]
fn manpager_指向自身时不覆盖_man_ai_pager() {
    let dir = TempDir::new("shell-self");
    let run = run_shell(&dir, "ls", &[("MANPAGER", BIN)], 0);
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai");
    assert_eq!(record_value(&run.record, "MAN_AI_PAGER"), "<unset>");

    // 经符号链接解析后仍是 man-ai 自身。
    let link = dir.path().join("man-ai-link");
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    let run = run_shell(
        &dir,
        "ls",
        &[
            ("MAN_AI_PAGER", "cat"),
            ("MANPAGER", &link.display().to_string()),
        ],
        0,
    );
    assert_eq!(record_value(&run.record, "MAN_AI_PAGER"), "cat");
}

#[test]
fn 双破折号之后的自有参数照原样交给_man() {
    let dir = TempDir::new("shell-boundary");
    let run = run_shell(&dir, "-- --lang zh-CN --translate", &[], 0);
    assert_eq!(recorded_args(&run.record), "-- --lang zh-CN --translate");
    assert_eq!(record_value(&run.record, "MANPAGER"), "man-ai");
}

#[test]
fn 未知参数与短选项原样交给_man() {
    let dir = TempDir::new("shell-passthru-args");
    let run = run_shell(&dir, "-h --bogus -V", &[], 0);
    assert_eq!(recorded_args(&run.record), "-h --bogus -V");
}

// ---------------------------------------------------------------- 薄壳模式：让路与透传

#[test]
fn p_覆盖时让路且不设_manpager() {
    let dir = TempDir::new("shell-p");
    let run = run_shell(
        &dir,
        "-P 'less -X' --translate ls",
        &[("MANPAGER", "less -sR")],
        0,
    );
    assert_eq!(run.code, 0);
    assert_eq!(
        run.stderr,
        "man-ai: -P overrides MANPAGER; not translating\n"
    );
    assert_eq!(recorded_args(&run.record), "-P less -X ls");
    assert_eq!(record_value(&run.record, "MANPAGER"), "less -sR");
    assert_eq!(record_value(&run.record, "MAN_AI_PAGER"), "<unset>");

    for argv in ["--pager less ls", "--pager=less ls", "-aP less ls"] {
        let run = run_shell(&dir, argv, &[], 0);
        assert_eq!(
            run.stderr, "man-ai: -P overrides MANPAGER; not translating\n",
            "{argv}"
        );
        assert_eq!(record_value(&run.record, "MANPAGER"), "<unset>", "{argv}");
    }
}

#[test]
fn 透传入口不修改_manpager_且退出码沿用_man() {
    let dir = TempDir::new("shell-w");
    for argv in ["-w ls", "-M /x ls", "-S 1 ls", "-t ls", "-T ps ls"] {
        let run = run_shell(&dir, argv, &[("MANPAGER", "less -sR")], 7);
        assert_eq!(run.code, 7, "{argv} 应透传退出码");
        assert_eq!(record_value(&run.record, "MANPAGER"), "less -sR", "{argv}");
        assert_eq!(
            record_value(&run.record, "MAN_AI_PAGER"),
            "<unset>",
            "{argv}"
        );
    }
}

#[test]
fn 透传入口摘掉自有参数() {
    let dir = TempDir::new("shell-w-strip");
    let run = run_shell(&dir, "--lang ja -w ls", &[], 0);
    assert_eq!(recorded_args(&run.record), "-w ls");
    assert_eq!(record_value(&run.record, "MANPAGER"), "<unset>");
}

#[test]
fn 找不到_man_时退出_127() {
    let dir = TempDir::new("t18-no-man");
    let (_, code) = run_pty_command(
        &dir,
        &format!("'{BIN}' ls"),
        &[("PATH", "/nonexistent"), ("TERM", "dumb")],
    );
    assert_eq!(code, 127);
}

// ---------------------------------------------------------------- 内层分页器链

/// 在 pty 下跑分页器模式：stdin 是管道，stdout 是终端。
fn run_pager_mode(dir: &TempDir, args: &str, extra_env: &[(&str, &str)], input: &str) -> PtyOut {
    let stderr_file = dir.file("pager-stderr.txt");
    let script = format!(
        "printf '%s' '{input}' | '{BIN}' {args} 2>'{err}'\n",
        err = stderr_file.display()
    );
    let mut env = vec![("HOME", dir.path().to_str().unwrap()), ("TERM", "dumb")];
    env.extend_from_slice(extra_env);
    let (mut output, code) = run_pty_command(dir, &script, &env);
    let stderr = fs::read_to_string(&stderr_file).unwrap_or_default();
    // 让调用方通过环境读到 stderr：warning 走文件，正文走 pty。
    if !stderr.is_empty() {
        output.extend_from_slice(format!("\n[stderr]{stderr}").as_bytes());
    }
    PtyOut { output, code }
}

#[test]
fn 分页器链追加剩余_argv_并原样直通() {
    let dir = TempDir::new("pager-args");
    let pager = fake_pager(&dir, "pager", 0);
    let extra = [("MAN_AI_PAGER", pager.to_str().unwrap())];
    let pty = run_pager_mode(
        &dir,
        "--lang ja --no-translate -X -- --translate",
        &extra,
        "page line\n",
    );
    assert_eq!(pty.code, 0);
    assert_eq!(pty_text(&pty.output), "page line\n");
    assert_eq!(
        fs::read_to_string(dir.path().join("pager-argv.txt")).unwrap(),
        "-X -- --translate"
    );
}

#[test]
fn 分页器链按_man_ai_pager_再_pager() {
    let dir = TempDir::new("pager-order");
    let pager = fake_pager(&dir, "pager", 0);
    let pty = run_pager_mode(&dir, "", &[("PAGER", pager.to_str().unwrap())], "x\n");
    assert_eq!(pty.code, 0);
    assert_eq!(pty_text(&pty.output), "x\n");
    assert_eq!(
        fs::read_to_string(dir.path().join("pager-argv.txt")).unwrap(),
        ""
    );

    // MAN_AI_PAGER 优先于 PAGER。
    let first = fake_pager(&dir, "first", 0);
    let second = fake_pager(&dir, "second", 0);
    let pty = run_pager_mode(
        &dir,
        "",
        &[
            ("MAN_AI_PAGER", first.to_str().unwrap()),
            ("PAGER", second.to_str().unwrap()),
        ],
        "y\n",
    );
    assert_eq!(pty.code, 0);
    assert!(dir.path().join("first-argv.txt").exists());
    assert!(!dir.path().join("second-argv.txt").exists());
}

#[test]
fn 拆词失败警告并跳到下一候选() {
    let dir = TempDir::new("pager-badquote");
    let pager = fake_pager(&dir, "pager", 0);
    let pty = run_pager_mode(
        &dir,
        "",
        &[
            ("MAN_AI_PAGER", "pager 'unclosed"),
            ("PAGER", pager.to_str().unwrap()),
        ],
        "ok\n",
    );
    assert_eq!(pty.code, 0);
    let text = pty_text(&pty.output);
    assert!(text.contains("cannot split inner pager command"), "{text}");
    assert!(text.contains("ok\n"), "{text}");
    assert!(dir.path().join("pager-argv.txt").exists());
}

#[test]
fn 递归指向自身时跳到下一候选() {
    let dir = TempDir::new("pager-loop");
    let link = dir.path().join("man-ai");
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    let pager = fake_pager(&dir, "pager", 0);
    let pty = run_pager_mode(
        &dir,
        "",
        &[
            ("MAN_AI_PAGER", link.to_str().unwrap()),
            ("PAGER", pager.to_str().unwrap()),
        ],
        "loop\n",
    );
    assert_eq!(pty.code, 0);
    assert_eq!(pty_text(&pty.output), "loop\n");
    assert!(dir.path().join("pager-argv.txt").exists());
}

#[test]
fn 整条链不可用时直写_stdout_并退出零() {
    let dir = TempDir::new("pager-none");
    let pty = run_pager_mode(&dir, "", &[("PATH", "/nonexistent")], "裸写\n");
    assert_eq!(pty.code, 0);
    assert_eq!(pty_text(&pty.output), "裸写\n");
}

#[test]
fn 分页器退出码透传() {
    let dir = TempDir::new("pager-exit");
    let pager = fake_pager(&dir, "pager", 3);
    let pty = run_pager_mode(
        &dir,
        "",
        &[("MAN_AI_PAGER", pager.to_str().unwrap())],
        "x\n",
    );
    assert_eq!(pty.code, 3);
}

#[test]
fn 分页器被信号杀死时为_128_加信号() {
    let dir = TempDir::new("pager-signal");
    write_script(&dir.path().join("killer"), "#!/bin/sh\nkill -TERM $$\n");
    let pty = run_pager_mode(
        &dir,
        "",
        &[("MAN_AI_PAGER", dir.path().join("killer").to_str().unwrap())],
        "x\n",
    );
    assert_eq!(pty.code, 128 + 15);
}

// ---------------------------------------------------------------- 与平台 man 的字节差分

#[test]
fn 不翻译路径与平台_man_字节一致() {
    let Some(real) = RealMan::new("t18-diff-page") else {
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
    let (ours, ours_stderr, ours_code) = real.run(
        "ours",
        &real.man_command(page),
        &[
            ("MANPAGER", "man-ai --no-translate"),
            ("MAN_AI_PAGER", "cat"),
        ],
        "C.UTF-8",
    );

    assert_eq!(baseline_code, 0, "基线 man 失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "基线 man 写了 stderr");
    assert!(!baseline.is_empty(), "基线没有输出，fixture 页可能没找到");
    assert_eq!(ours_stderr, "", "man-ai 路径写了 stderr");
    assert_eq!(ours_code, baseline_code, "退出码应与平台 man 一致");
    assert_bytes_eq(&ours, &baseline, "不翻译路径");
}

#[test]
fn 透传入口_w_与_t_的输出与平台_man_字节一致() {
    let Some(real) = RealMan::new("t18-diff-entries") else {
        eprintln!("跳过：本机没有 man");
        return;
    };

    for (index, args) in ["-w man-ai-fixture", "-t man-ai-fixture"]
        .into_iter()
        .enumerate()
    {
        let (baseline, baseline_stderr, baseline_code) = real.run(
            &format!("base{index}"),
            &real.man_command(args),
            &[],
            "C.UTF-8",
        );
        let (ours, ours_stderr, ours_code) = real.run(
            &format!("ours{index}"),
            &format!("'{BIN}' {args}"),
            &[],
            "C.UTF-8",
        );
        assert_eq!(baseline_code, 0, "{args} 基线失败：{baseline_stderr}");
        assert_eq!(baseline_stderr, "", "{args} 基线写了 stderr");
        assert!(!baseline.is_empty(), "{args} 基线没有输出");
        assert_eq!(ours_stderr, "", "{args} man-ai 写了 stderr");
        assert_eq!(ours_code, baseline_code, "{args} 退出码应与 man 一致");
        assert_bytes_eq(&ours, &baseline, args);
    }
}
