//! #20 及后续票共用的集成测试工具：临时 XDG 配置、假 man、pty 运行、内嵌假模型服务。
//!
//! 每个测试文件都 `mod common;`，因此这里的工具按「可能未使用」处理。

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
pub fn model_config(model: &FakeModel, extra: &str) -> String {
    format!(
        "endpoint = \"{}/\"\nmodel = \"test-model\"\napi_key = \"sk-test\"\n{extra}",
        model.endpoint()
    )
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
    let run = run_pty_script(dir, &script, &extra);
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
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    let code = child.wait().unwrap().exit_code() as i32;
    RawPtyRun { output, code }
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
    requests: Mutex<Vec<Recorded>>,
}

pub struct FakeModel {
    endpoint: String,
    state: Arc<ModelState>,
    runtime: tokio::runtime::Runtime,
}

impl FakeModel {
    pub fn start(replies: Vec<Reply>) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let state = Arc::new(ModelState {
            replies: Mutex::new(VecDeque::from(replies)),
            requests: Mutex::new(Vec::new()),
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
    state.requests.lock().unwrap().push(Recorded {
        method,
        path,
        authorization,
        body,
    });
    let reply = state
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Reply::Status(
            500,
            json!({"error": {"message": "no reply scripted"}}),
        ));
    match reply {
        Reply::Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Reply::Status(code, value) => (status(code), Json(value)).into_response(),
        Reply::Raw(code, body) => (status(code), body).into_response(),
        Reply::Delay(delay, value) => {
            tokio::time::sleep(delay).await;
            (StatusCode::OK, Json(value)).into_response()
        }
    }
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
