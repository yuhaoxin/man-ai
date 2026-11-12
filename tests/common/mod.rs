//! 集成测试共用工具：临时 XDG 配置、假 man、pty 运行、内嵌假模型服务。
//!
//! 供 `mod common;` 的测试文件使用，因此这里的工具按「可能未使用」处理。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};

pub const BIN: &str = env!("CARGO_BIN_EXE_man-ai");

// ---------------------------------------------------------------- 临时目录

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "man-ai-t20-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn xdg(&self) -> PathBuf {
        self.0.join("xdg")
    }

    /// 写 `$XDG_CONFIG_HOME/man-ai/config.toml`。
    pub fn write_config(&self, text: &str) -> PathBuf {
        let dir = self.xdg().join("man-ai");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// 指向假模型服务的配置文件内容。
///
/// 默认 `concurrency = 1`：脚本化应答按请求到达顺序对应块，只有串行时
/// 这个对应关系才确定；并发测试用 [`model_config_with`]。
pub fn model_config(model: &FakeModel, extra: &str) -> String {
    model_config_with(model, 1, extra)
}

/// 指定并发度的配置内容。
pub fn model_config_with(model: &FakeModel, concurrency: usize, extra: &str) -> String {
    format!(
        "endpoint = \"{}/\"\nmodel = \"test-model\"\napi_key = \"sk-test\"\nconcurrency = {concurrency}\n{extra}",
        model.endpoint()
    )
}

/// 独立的 `XDG_CACHE_HOME`；缓存断言只看本次运行的目录。
pub fn cache_env(dir: &TempDir) -> Vec<(&'static str, String)> {
    let path = dir.file("cache");
    vec![("XDG_CACHE_HOME", path.to_str().unwrap().to_string())]
}

pub fn env_refs<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    env.iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

/// `v1` 下的缓存条目文件数（不含 tmp）。
pub fn entries(dir: &TempDir) -> usize {
    let v1 = dir.file("cache").join("man-ai").join("v1");
    fs::read_dir(&v1)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|shard| shard.file_name() != "tmp")
        .map(|shard| {
            fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .flatten()
                .count()
        })
        .sum()
}

// ---------------------------------------------------------------- 普通运行

pub struct Run {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

pub fn base_command(dir: &TempDir) -> Command {
    let mut command = Command::new(BIN);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.xdg())
        .env("TERM", "dumb")
        .env("MANWIDTH", "80");
    command
}

/// stdin 管道、stdout/stderr 也是管道：分页器模式的非 TTY 路径。
pub fn run_pipe(dir: &TempDir, args: &[&str], stdin: &[u8], extra_env: &[(&str, &str)]) -> Run {
    let mut command = base_command(dir);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    into_run(child.wait_with_output().unwrap())
}

fn into_run(output: Output) -> Run {
    Run {
        stdout: output.stdout,
        stderr: output.stderr,
        code: output.status.code().unwrap_or(-1),
    }
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------- pty 运行

pub struct PtyRun {
    pub output: Vec<u8>,
    pub stderr: String,
    pub code: i32,
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 分页器模式 + stdout 是 TTY：stdin 从文件重定向（非 TTY），页面交给内层分页器。
///
/// 内层分页器用 `MAN_AI_PAGER=cat`，输出与 stdin 字节一致。
pub fn run_pager_tty(
    dir: &TempDir,
    args: &[&str],
    stdin: &[u8],
    extra_env: &[(&str, &str)],
) -> PtyRun {
    run_pager_tty_with_pid(dir, args, stdin, extra_env, |_| {})
}

/// 同上，但在读输出期间把 man-ai 的 pid 交给 `hook`（发信号、等文件标记用）。
pub fn run_pager_tty_with_pid(
    dir: &TempDir,
    args: &[&str],
    stdin: &[u8],
    extra_env: &[(&str, &str)],
    hook: impl FnOnce(u32) + Send + 'static,
) -> PtyRun {
    let input = dir.file("stdin.bin");
    fs::write(&input, stdin).unwrap();
    let stderr_file = dir.file("stderr.txt");
    let mut script = String::from("exec ");
    script.push_str(&sh_quote(BIN));
    for arg in args {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push_str(&format!(
        " <{} 2>{}",
        sh_quote(input.to_str().unwrap()),
        sh_quote(stderr_file.to_str().unwrap())
    ));
    let mut extra = vec![("MAN_AI_PAGER", "cat")];
    extra.extend_from_slice(extra_env);
    let run = run_pty_script_with(dir, &script, &extra, hook);
    PtyRun {
        output: strip_carriage_returns(run.output),
        stderr: fs::read_to_string(&stderr_file).unwrap_or_default(),
        code: run.code,
    }
}

/// pty 的输出换行是 `\r\n`，比较前归一成 `\n`。
fn strip_carriage_returns(bytes: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            index += 1;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// 在 pty 下跑一条 shell 命令，输出去掉 pty 的 `\r`；差分测试用。
///
/// 命令自己负责重定向 stderr（通常重定向到 `dir.file(...)`），因为这里只回传 pty 输出。
pub fn run_pty_command(dir: &TempDir, script: &str, extra_env: &[(&str, &str)]) -> (Vec<u8>, i32) {
    let run = run_pty_script_with(dir, script, extra_env, |_| {});
    (strip_carriage_returns(run.output), run.code)
}

/// 薄壳模式 + stdout 是 TTY：stdin/stdout 都在 pty 上，stderr 重定向到文件。
pub fn run_shell_tty(dir: &TempDir, args: &[&str], extra_env: &[(&str, &str)]) -> PtyRun {
    let stderr_file = dir.file("stderr.txt");
    let mut script = String::from("exec ");
    script.push_str(&sh_quote(BIN));
    for arg in args {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push_str(&format!(" 2>{}", sh_quote(stderr_file.to_str().unwrap())));
    let run = run_pty_script(dir, &script, extra_env);
    PtyRun {
        output: strip_carriage_returns(run.output),
        stderr: fs::read_to_string(&stderr_file).unwrap_or_default(),
        code: run.code,
    }
}

/// 薄壳模式 + stdout 非 TTY：stdin 是 pty，stdout/stderr 重定向到文件。
pub fn run_shell_capture(dir: &TempDir, args: &[&str], extra_env: &[(&str, &str)]) -> PtyRun {
    let stdout_file = dir.file("stdout.txt");
    let stderr_file = dir.file("stderr.txt");
    let mut script = String::from("exec ");
    script.push_str(&sh_quote(BIN));
    for arg in args {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push_str(&format!(
        " >{} 2>{}",
        sh_quote(stdout_file.to_str().unwrap()),
        sh_quote(stderr_file.to_str().unwrap())
    ));
    let run = run_pty_script(dir, &script, extra_env);
    PtyRun {
        output: fs::read(stdout_file).unwrap_or_default(),
        stderr: fs::read_to_string(stderr_file).unwrap_or_default(),
        code: run.code,
    }
}

struct RawPtyRun {
    output: Vec<u8>,
    code: i32,
}

fn run_pty_script(dir: &TempDir, script: &str, extra_env: &[(&str, &str)]) -> RawPtyRun {
    run_pty_script_with(dir, script, extra_env, |_| {})
}

fn run_pty_script_with(
    dir: &TempDir,
    script: &str,
    extra_env: &[(&str, &str)],
    hook: impl FnOnce(u32) + Send + 'static,
) -> RawPtyRun {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", script]);
    command.env_clear();
    command.env("PATH", "/usr/bin:/bin");
    command.env("HOME", dir.path().to_str().unwrap());
    command.env("XDG_CONFIG_HOME", dir.xdg().to_str().unwrap());
    command.env("TERM", "dumb");
    command.env("MANWIDTH", "80");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let pid = child.process_id();
    let background = pid.map(|pid| std::thread::spawn(move || hook(pid)));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    let code = child.wait().unwrap().exit_code() as i32;
    if let Some(background) = background {
        let _ = background.join();
    }
    RawPtyRun { output, code }
}

// ---------------------------------------------------------------- 假分页器

/// 边读边记录的内层分页器：启动时写 `<name>.start` 标记（证明首屏），
/// 每读到一行同时追加到 `<name>.log` 并回显到 stdout。
pub fn recording_pager(dir: &TempDir, name: &str) -> PathBuf {
    let start = dir.file(&format!("{name}.start"));
    let log = dir.file(&format!("{name}.log"));
    let path = dir.file(&format!("{name}-pager"));
    write_script(
        &path,
        &format!(
            "#!/bin/sh\necho started > '{}'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> '{}'\n  printf '%s\\n' \"$line\"\ndone\n",
            start.display(),
            log.display()
        ),
    );
    path
}

/// 只读一个字节就退出的分页器：用于 EPIPE（退出码 7）。
pub fn quitting_pager(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.file(&format!("{name}-pager"));
    write_script(&path, "#!/bin/sh\nhead -c 1 >/dev/null\nexit 7\n");
    path
}

/// 记录分页器收到的行（含页首/尾行，pty 的 `\r\n` 已归一）。
pub fn pager_log(dir: &TempDir, name: &str) -> Vec<String> {
    fs::read_to_string(dir.file(&format!("{name}.log")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// 往进程发 SIGINT；`/bin/kill` 在 macOS 与 Linux 都有。
pub fn send_sigint(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

// ---------------------------------------------------------------- 假 man

/// 写一个假 `man`：把 `page` 输出到 stdout，并按 `code` 退出。
///
/// 同时把收到的 `MANPAGER` 记录到 `man-record.txt`。
pub fn fake_man(dir: &TempDir, page: &[u8], code: i32) -> PathBuf {
    let fixture = dir.file("page.bin");
    fs::write(&fixture, page).unwrap();
    let record = dir.file("man-record.txt");
    write_script(
        &dir.file("man"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"${{MANPAGER-<unset>}}\" > '{}'\ncat '{}'\nexit {code}\n",
            record.display(),
            fixture.display()
        ),
    );
    record
}

/// 写一个假 `man -k`：输出结果行，并把收到的 `WHATISPAGER` 记录到 `whatispager-record.txt`。
pub fn fake_kman(dir: &TempDir, output: &[u8], code: i32) -> PathBuf {
    let fixture = dir.file("kpage.bin");
    fs::write(&fixture, output).unwrap();
    let record = dir.file("whatispager-record.txt");
    write_script(
        &dir.file("man"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"${{WHATISPAGER-<unset>}}\" > '{}'\n/bin/cat '{}'\nexit {code}\n",
            record.display(),
            fixture.display()
        ),
    );
    record
}

/// 记录 man 实际收到的 argv 与关键环境变量，然后按给定退出码退出。
pub fn recording_man(dir: &TempDir, exit_code: i32) -> PathBuf {
    let record = dir.file("man-record.txt");
    write_script(
        &dir.file("man"),
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

/// 读 [`recording_man`] 记录文件里 `key=value` 的右值。
pub fn record_value(record: &Path, key: &str) -> String {
    let text = fs::read_to_string(record).unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or("<missing>")
        .to_string()
}

pub fn recorded_args(record: &Path) -> String {
    record_value(record, "args")
}

// ---------------------------------------------------------------- 真 man 差分

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
pub struct RealMan {
    pub dir: TempDir,
    man: PathBuf,
    pub base_path: String,
    pub manpath: PathBuf,
}

impl RealMan {
    /// 本机没有 `man` 时返回 `None`，调用方跳过。
    pub fn new(tag: &str) -> Option<Self> {
        let man = find_in_path("man")?;
        let dir = TempDir::new(tag);
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        // MANPAGER 里的 man-ai 从 PATH 解析（§4.2 只写二进制名）。
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

    /// 在 pty 下跑一条命令，stderr 落到 `dir` 里的独立文件。
    pub fn run(
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
            // groff 据此生成固定的 PostScript 头部；mandoc 忽略。
            ("SOURCE_DATE_EPOCH", "0"),
        ];
        env.extend_from_slice(extra_env);
        let (output, code) = run_pty_command(&self.dir, &script, &env);
        let stderr = fs::read_to_string(&stderr_file).unwrap_or_default();
        (output, stderr, code)
    }

    pub fn man_command(&self, args: &str) -> String {
        format!("{} {args}", shell_quote(&self.man))
    }

    /// fixture 页所在的临时 MANPATH 根。
    pub fn manpath(&self) -> &Path {
        &self.manpath
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

/// 逐字节比较 stdout；失败信息才做有损解码。
pub fn assert_bytes_eq(ours: &[u8], baseline: &[u8], label: &str) {
    assert!(
        ours == baseline,
        "{label}：stdout 应与平台 man 逐字节一致\nours={:?}\nman={:?}",
        String::from_utf8_lossy(ours),
        String::from_utf8_lossy(baseline)
    );
}

// ---------------------------------------------------------------- 假模型服务

#[derive(Clone)]
pub enum Reply {
    /// 200 + JSON。
    Ok(Value),
    /// 指定状态码 + JSON。
    Status(u16, Value),
    /// 指定状态码 + 原始文本。
    Raw(u16, String),
    /// 睡一段时间再回 200 + JSON（用于触发客户端超时）。
    Delay(Duration, Value),
    /// 等指定文件出现再回 200 + JSON：让慢块由测试观察到的同步信号放行，而不是固定计时窗口。
    AwaitFile(PathBuf, Duration, Value),
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
}

struct ModelState {
    replies: Mutex<VecDeque<Reply>>,
    /// 按请求里的 user 内容应答；与请求到达顺序无关，用于并发测试。
    script: std::collections::HashMap<String, Reply>,
    requests: Mutex<Vec<Recorded>>,
    in_flight: AtomicU32,
    peak: AtomicU32,
}

pub struct FakeModel {
    endpoint: String,
    state: Arc<ModelState>,
    runtime: tokio::runtime::Runtime,
}

impl FakeModel {
    pub fn start(replies: Vec<Reply>) -> Self {
        Self::start_with(replies, Vec::new())
    }

    /// 按请求内容应答：同一内容每次得到同一响应，适合并发下的顺序断言。
    pub fn start_by_content(script: Vec<(String, Reply)>) -> Self {
        Self::start_with(Vec::new(), script)
    }

    fn start_with(replies: Vec<Reply>, script: Vec<(String, Reply)>) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let state = Arc::new(ModelState {
            replies: Mutex::new(VecDeque::from(replies)),
            script: script.into_iter().collect(),
            requests: Mutex::new(Vec::new()),
            in_flight: AtomicU32::new(0),
            peak: AtomicU32::new(0),
        });
        let endpoint = runtime.block_on({
            let state = state.clone();
            async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let app = Router::new()
                    .fallback(any(handle_model_request))
                    .with_state(state);
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                format!("http://{address}/v1")
            }
        });
        Self {
            endpoint,
            state,
            runtime,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    /// 观察到的最大在途请求数（并发上限断言用）。
    pub fn peak_in_flight(&self) -> usize {
        self.state.peak.load(Ordering::SeqCst) as usize
    }
}

async fn handle_model_request(State(state): State<Arc<ModelState>>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = to_bytes(request.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let key = body["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    state.requests.lock().unwrap().push(Recorded {
        method,
        path,
        authorization,
        body,
    });
    let scripted = state.script.get(&key).cloned();
    let reply = scripted
        .or_else(|| state.replies.lock().unwrap().pop_front())
        .unwrap_or(Reply::Status(
            500,
            json!({"error": {"message": "no reply scripted"}}),
        ));
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak.fetch_max(now, Ordering::SeqCst);
    let response = match reply {
        Reply::Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Reply::Status(code, value) => (status(code), Json(value)).into_response(),
        Reply::Raw(code, body) => (status(code), body).into_response(),
        Reply::Delay(delay, value) => {
            tokio::time::sleep(delay).await;
            (StatusCode::OK, Json(value)).into_response()
        }
        Reply::AwaitFile(path, timeout, value) => {
            let deadline = tokio::time::Instant::now() + timeout;
            while !path.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (StatusCode::OK, Json(value)).into_response()
        }
    };
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    response
}

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap()
}

/// 一个 200 响应体：`choices[0].message.content`。
pub fn completion(content: &str, finish_reason: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": finish_reason
        }]
    })
}

pub fn error_body(code: &str, kind: &str) -> Value {
    json!({"error": {"message": "scripted failure", "type": kind, "code": code}})
}

/// 写一半就关连接的服务：模拟连接中断。
pub struct DisconnectServer {
    endpoint: String,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    runtime: tokio::runtime::Runtime,
}

impl DisconnectServer {
    pub fn start() -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let endpoint = runtime.block_on({
            let connections = connections.clone();
            async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let connections = connections.clone();
                    tokio::spawn(async move {
                        connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let mut buffer = [0u8; 4096];
                        let _ = socket.read(&mut buffer).await;
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\"id\":\"x\",",
                            )
                            .await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
                format!("http://{address}/v1")
            }
        });
        Self {
            endpoint,
            connections,
            runtime,
        }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}
