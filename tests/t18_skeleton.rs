//! #18 透传骨架的集成测试：CLI、薄壳模式、分页器模式与真 man 差分。
//!
//! 薄壳模式的判定依赖 stdin 是 TTY，分页器链依赖 stdout 是 TTY，两者都只能
//! 在 pty 下观察；pty 里的 `sh -c` 让需要方向的一端接管道。

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
            "man-ai-t18-{tag}-{}-{}",
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

fn envs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 非 TTY 运行：stdin/stdout/stderr 都是管道。
fn run(args: &[&str], extra_env: &[(&str, &str)], stdin: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env_remove("MANPAGER")
        .env_remove("MAN_AI_PAGER")
        .env_remove("PAGER")
        .envs(extra_env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

struct PtyOut {
    output: Vec<u8>,
    code: i32,
}

/// 在 pty 下运行脚本：stdin 与 stdout 都是终端。
fn run_pty(script: &str, extra_env: &[(String, String)]) -> PtyOut {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", script]);
    command.env_clear();
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    let status = child.wait().unwrap();
    PtyOut {
        output,
        code: status.exit_code() as i32,
    }
}

/// pty 会把 `\n` 写成 `\r\n`，比较文本前统一掉。
fn pty_text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).replace("\r\n", "\n")
}

// ---------------------------------------------------------------- 假 man / 假分页器

/// 记录 man 实际收到的 argv 与三个关键环境变量，然后按给定退出码退出。
fn fake_man(dir: &Path, exit_code: i32) -> PathBuf {
    let record = dir.join("man-record.txt");
    write_script(
        &dir.join("man"),
        &format!(
            "#!/bin/sh\n{{\n\
             echo \"args=$*\"\n\
             echo \"MANPAGER=${{MANPAGER-<unset>}}\"\n\
             echo \"MAN_AI_PAGER=${{MAN_AI_PAGER-<unset>}}\"\n\
             echo \"MAN_KEEP_FORMATTING=${{MAN_KEEP_FORMATTING-<unset>}}\"\n\
             }} > '{record}'\nexit {exit_code}\n",
            record = record.display()
        ),
    );
    record
}

/// 假内层分页器：记录 argv，原样输出 stdin，按给定退出码退出。
fn fake_pager(dir: &Path, name: &str, exit_code: i32) -> PathBuf {
    let argv_file = dir.join(format!("{name}-argv.txt"));
    let script = dir.join(name);
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{argv}'\ncat\nexit {exit_code}\n",
            argv = argv_file.display()
        ),
    );
    script
}

fn record_value(record: &Path, key: &str) -> String {
    let text = fs::read_to_string(record).unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or("<missing>")
        .to_string()
}

fn recorded_args(record: &Path) -> String {
    record_value(record, "args")
}

struct ShellRun {
    pty: Vec<u8>,
    code: i32,
    stderr: String,
    record: PathBuf,
}

/// 在 pty 下以薄壳模式运行 `man-ai <args>`，PATH 里放假 man。
fn run_shell(dir: &TempDir, args: &str, extra_env: &[(&str, &str)], man_exit: i32) -> ShellRun {
    let record = fake_man(dir.path(), man_exit);
    let stderr_file = dir.path().join("man-ai-stderr.txt");
    let path = format!("{}:/usr/bin:/bin", dir.path().display());
    let script = format!("'{BIN}' {args} 2>'{err}'\n", err = stderr_file.display());
    let mut all_env = envs(&[
        ("PATH", &path),
        ("HOME", &dir.path().display().to_string()),
        ("TERM", "dumb"),
    ]);
    all_env.extend(envs(extra_env));
    let pty = run_pty(&script, &all_env);
    ShellRun {
        pty: pty.output,
        code: pty.code,
        stderr: fs::read_to_string(&stderr_file).unwrap_or_default(),
        record,
    }
}

// ---------------------------------------------------------------- --help / --version

#[test]
fn help_打印用法与五个自有参数() {
    let out = run(&["--help"], &[], b"");
    assert!(out.status.success());
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
    let out = run(&["--version"], &[], b"");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("man-ai {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn help_与_version_优先于语言校验与入口判定() {
    for argv in [
        vec!["--lang", "非法 值", "--help"],
        vec!["-P", "less", "--version"],
        vec!["--lang", "--help"],
    ] {
        let out = run(&argv, &[], b"");
        assert_eq!(out.status.code(), Some(0), "{argv:?} 应成功");
        assert!(!out.stdout.is_empty());
    }
}

#[test]
fn lang_缺值空值形状非法退出二() {
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
        let out = run(&argv, &[], b"");
        assert_eq!(out.status.code(), Some(2), "{argv:?} 应退出 2");
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
    let out = run(
        &["--lang", "zh_CN.UTF-8", "--no-translate"],
        &[],
        b"page bytes\n",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"page bytes\n");
    assert!(out.stderr.is_empty());
}

// ---------------------------------------------------------------- 分页器模式

#[test]
fn 分页器模式读完_stdin_并直写_stdout() {
    let input: &[u8] = b"LS(1)\n\n     l\x1bls\x08s - list\n\n\nend without newline";
    let out = run(&[], &[], input);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, input);
    assert!(out.stderr.is_empty());
}

#[test]
fn 分页器模式忽略_stdin_为空的情形() {
    let out = run(&[], &[], b"");
    assert_eq!(out.status.code(), Some(0));
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
    let pty = run_pty(
        &format!("'{BIN}' ls"),
        &envs(&[("PATH", "/nonexistent"), ("TERM", "dumb")]),
    );
    assert_eq!(pty.code, 127);
}

// ---------------------------------------------------------------- 内层分页器链

/// 在 pty 下跑分页器模式：stdin 是管道，stdout 是终端。
fn run_pager_mode(dir: &TempDir, args: &str, extra_env: &[(&str, &str)], input: &str) -> PtyOut {
    let stderr_file = dir.path().join("pager-stderr.txt");
    let script = format!(
        "printf '%s' '{input}' | '{BIN}' {args} 2>'{err}'\n",
        err = stderr_file.display()
    );
    let mut all_env = envs(&[
        ("HOME", &dir.path().display().to_string()),
        ("TERM", "dumb"),
    ]);
    all_env.extend(envs(extra_env));
    let pty = run_pty(&script, &all_env);
    let stderr = fs::read_to_string(&stderr_file).unwrap_or_default();
    let mut output = pty.output;
    // 让调用方通过环境读到 stderr：warning 走文件，正文走 pty。
    if !stderr.is_empty() {
        output.extend_from_slice(format!("\n[stderr]{stderr}").as_bytes());
    }
    PtyOut {
        output,
        code: pty.code,
    }
}

#[test]
fn 分页器链追加剩余_argv_并原样直通() {
    let dir = TempDir::new("pager-args");
    let pager = fake_pager(dir.path(), "pager", 0);
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
    let pager = fake_pager(dir.path(), "pager", 0);
    let pty = run_pager_mode(&dir, "", &[("PAGER", pager.to_str().unwrap())], "x\n");
    assert_eq!(pty.code, 0);
    assert_eq!(pty_text(&pty.output), "x\n");
    assert_eq!(
        fs::read_to_string(dir.path().join("pager-argv.txt")).unwrap(),
        ""
    );

    // MAN_AI_PAGER 优先于 PAGER。
    let first = fake_pager(dir.path(), "first", 0);
    let second = fake_pager(dir.path(), "second", 0);
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
    let pager = fake_pager(dir.path(), "pager", 0);
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
    let pager = fake_pager(dir.path(), "pager", 0);
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
    let pager = fake_pager(dir.path(), "pager", 3);
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

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

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
         A second paragraph keeps the page longer than one block.\n",
    )
    .unwrap();
    root.join("man")
}

/// 真 man + 固定 `MANPATH` 的差分环境；基线每次由本机 man 现场生成。
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
        // man 通过 MANPAGER 调用的 man-ai 从 PATH 解析（§4.2 只写二进制名）。
        write_script(
            &bin_dir.join("man-ai"),
            &format!("#!/bin/sh\nexec '{BIN}' \"$@\"\n"),
        );
        let manpath = fixture_manpath(dir.path());
        let base_path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Some(RealMan {
            dir,
            man,
            base_path,
            manpath,
        })
    }

    /// 在 pty 下跑一条命令，stderr 落到单独文件；返回输出与 stderr。
    fn run(
        &self,
        stderr_name: &str,
        command: &str,
        extra_env: &[(&str, &str)],
    ) -> (PtyOut, String) {
        let stderr_file = self.dir.path().join(stderr_name);
        let script = format!("{command} 2>'{err}'\n", err = stderr_file.display());
        let mut all_env = envs(&[
            ("PATH", &self.base_path),
            ("HOME", &self.dir.path().display().to_string()),
            ("MANPATH", &self.manpath.display().to_string()),
            ("MANWIDTH", "80"),
            ("LC_ALL", "C.UTF-8"),
            ("TERM", "dumb"),
            // groff 据此生成固定的 PostScript 头部；mandoc 忽略。
            ("SOURCE_DATE_EPOCH", "0"),
        ]);
        all_env.extend(envs(extra_env));
        let pty = run_pty(&script, &all_env);
        (
            PtyOut {
                output: pty.output,
                code: pty.code,
            },
            fs::read_to_string(&stderr_file).unwrap_or_default(),
        )
    }

    fn man_command(&self, args: &str) -> String {
        format!("'{man}' {args}", man = self.man.display())
    }
}

#[test]
fn 不翻译路径与平台_man_字节一致() {
    let Some(real) = RealMan::new("diff-page") else {
        eprintln!("跳过：本机没有 man");
        return;
    };
    let page = "1 man-ai-fixture";

    let (baseline, baseline_stderr) = real.run(
        "err-baseline.txt",
        &real.man_command(page),
        &[("MANPAGER", "cat")],
    );
    let (ours, ours_stderr) = real.run(
        "err-ours.txt",
        &real.man_command(page),
        &[
            ("MANPAGER", "man-ai --no-translate"),
            ("MAN_AI_PAGER", "cat"),
        ],
    );

    assert_eq!(baseline.code, 0, "基线 man 失败：{baseline_stderr}");
    assert_eq!(baseline_stderr, "", "基线 man 写了 stderr");
    assert!(
        !baseline.output.is_empty(),
        "基线没有输出，fixture 页可能没找到"
    );
    assert_eq!(ours_stderr, "", "man-ai 路径写了 stderr");
    assert_eq!(ours.code, baseline.code, "退出码应与平台 man 一致");
    assert_eq!(
        String::from_utf8_lossy(&ours.output),
        String::from_utf8_lossy(&baseline.output),
        "stdout 应与平台 man 逐字节一致"
    );
}

#[test]
fn 透传入口_w_与_t_的输出与平台_man_字节一致() {
    let Some(real) = RealMan::new("diff-entries") else {
        eprintln!("跳过：本机没有 man");
        return;
    };

    for args in ["-w man-ai-fixture", "-t man-ai-fixture"] {
        let (baseline, baseline_stderr) =
            real.run(&format!("err-base{args}.txt"), &real.man_command(args), &[]);
        let (ours, ours_stderr) = real.run(
            &format!("err-ours{args}.txt"),
            &format!("'{BIN}' {args}"),
            &[],
        );
        assert_eq!(baseline.code, 0, "{args} 基线失败：{baseline_stderr}");
        assert_eq!(baseline_stderr, "", "{args} 基线写了 stderr");
        assert!(!baseline.output.is_empty(), "{args} 基线没有输出");
        assert_eq!(ours_stderr, "", "{args} man-ai 写了 stderr");
        assert_eq!(ours.code, baseline.code, "{args} 退出码应与 man 一致");
        assert_eq!(
            String::from_utf8_lossy(&ours.output),
            String::from_utf8_lossy(&baseline.output),
            "{args} stdout 应与 man 逐字节一致"
        );
    }
}
