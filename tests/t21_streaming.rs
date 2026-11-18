//! 非 TTY 流式路径与 EPIPE（§5.3、§5.4）。
//!
//! 按序整块出现、占位行、首尾行在阅读器单元层与 t26_reader 的 PTY 契约里断言。

mod common;

use std::process::{Command, Stdio};

use common::{FakeModel, Reply, TempDir, completion, model_config_with, run_pipe, text};

const LANG: &str = "zh-CN";

fn page(names: &[String]) -> String {
    format!("{}\n", names.join("\n\n"))
}

fn echo_script(names: &[String]) -> Vec<(String, Reply)> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            (
                format!("{name}\n"),
                Reply::Ok(completion(&format!("t{index}\n"), "stop")),
            )
        })
        .collect()
}

#[test]
fn 非_tty_的_translate_按序写出且无首尾行() {
    let names: Vec<String> = (0..3).map(|index| format!("b{index}")).collect();
    let model = FakeModel::start_by_content(echo_script(&names));
    let dir = TempDir::new("t21-pipe");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_pipe(
        &dir,
        &["--translate", "--lang", LANG],
        page(&names).as_bytes(),
        &[],
    );
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "t0\n\nt1\n\nt2\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}

#[test]
fn epipe_停止请求与写入并静默退出零() {
    // 每块译文 1000 字节、共 300 块：写出总量远超管道缓冲（macOS 约 76 KB），
    // `head -c` 读一点就退出，后续写入必然 EPIPE。
    let translation = format!("{}\n", "y".repeat(1000));
    let name = "x".to_string();
    let names: Vec<String> = (0..300).map(|_| name.clone()).collect();
    let model = FakeModel::start_by_content(vec![(
        format!("{name}\n"),
        Reply::Ok(completion(&translation, "stop")),
    )]);
    let dir = TempDir::new("t21-epipe");
    dir.write_config(&model_config_with(&model, 4, ""));
    let head = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "printf '%s' '{input}' | '{bin}' --translate --lang {lang} | head -c 50",
            input = page(&names),
            bin = common::BIN,
            lang = LANG,
        ))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.xdg())
        .env("TERM", "dumb")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    // EPIPE 静默退出 0：管道整体退出码是 head 的。
    assert_eq!(head.status.code(), Some(0), "{}", text(&head.stderr));
    assert!(head.stderr.is_empty(), "{}", text(&head.stderr));
    // 请求在写端坏掉后停止：远少于 300 块。
    assert!(model.count() < 150, "{}", model.count());
}

#[test]
fn 思考字段不进入输出与诊断() {
    let model = FakeModel::start(vec![Reply::Ok(serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "译文\n",
                "reasoning_content": "chain of thought SECRET",
                "thinking": "more SECRET"
            },
            "finish_reason": "stop"
        }]
    }))]);
    let dir = TempDir::new("t21-reasoning");
    dir.write_config(&model_config_with(&model, 1, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "译文\n");
    assert!(
        !text(&run.stderr).contains("SECRET"),
        "{}",
        text(&run.stderr)
    );
}
