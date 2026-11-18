//! 假 SSE 端点下的流式采用、重试与回退（§5.3、§7.4、§7.5）。
//!
//! 按序整块出现、占位行、首尾行在阅读器单元层与 t26_reader 的 PTY 契约里断言。

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::{
    FakeModel, Reply, TempDir, chunk, completion, model_config, model_config_with, run_pipe,
    text, usage_chunk,
};
use serde_json::json;

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

/// 一条「内容 delta → finish 帧 → 独立 usage 块 → [DONE]」的 SSE 应答。
fn sse_reply(content: &str, finish: Option<&str>, usage: Option<serde_json::Value>) -> Reply {
    let mut frames = vec![(Duration::ZERO, chunk(Some(content), None, None))];
    if let Some(finish) = finish {
        frames.push((Duration::ZERO, chunk(None, Some(finish), None)));
    }
    if let Some(usage) = usage {
        frames.push((Duration::ZERO, usage_chunk(usage)));
    }
    Reply::Sse(frames, true)
}

fn usage_value(prompt: u32, completion: u32, cached: Option<u32>) -> serde_json::Value {
    let mut usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    });
    if let Some(cached) = cached {
        usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
    }
    usage
}

#[test]
fn 请求体固定流式并带_include_usage() {
    let model = FakeModel::start(vec![sse_reply("译文\n", Some("stop"), None)]);
    let dir = TempDir::new("t21-stream-body");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "译文\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
    let body = &model.requests()[0].body;
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["stream_options"],
        json!({"include_usage": true})
    );
}

#[test]
fn 内容分块_delta_逐帧累积后采用() {
    // 内容拆成多个 delta 帧：客户端逐帧累积出完整译文。
    let reply = Reply::Sse(
        vec![
            (Duration::ZERO, chunk(Some("第一"), None, None)),
            (Duration::ZERO, chunk(Some("段\n"), None, None)),
            (Duration::ZERO, chunk(None, Some("stop"), None)),
            (Duration::ZERO, usage_chunk(usage_value(10, 2, Some(4)))),
        ],
        true,
    );
    let model = FakeModel::start(vec![reply]);
    let dir = TempDir::new("t21-delta-split");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(text(&run.stdout), "第一段\n");
    assert_eq!(run.code, 0);
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}

#[test]
fn usage_挂在使用内容块上或缺失都能采用() {
    // DeepSeek 形态：usage 与 finish_reason 同块。
    let rider = Reply::Sse(
        vec![
            (Duration::ZERO, chunk(Some("甲\n"), None, None)),
            (
                Duration::ZERO,
                chunk(None, Some("stop"), Some(usage_value(9, 1, None))),
            ),
        ],
        true,
    );
    let model = FakeModel::start(vec![rider]);
    let dir = TempDir::new("t21-usage-rider");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &[]);
    assert_eq!(text(&run.stdout), "甲\n");

    // usage 缺失：照常采用，不按 0 补、不报错。
    let nousage = FakeModel::start(vec![sse_reply("乙\n", Some("stop"), None)]);
    let dir = TempDir::new("t21-usage-missing");
    dir.write_config(&model_config(&nousage, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n", &[]);
    assert_eq!(text(&run.stdout), "乙\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}

#[test]
fn 中途_error_帧重试一次后回退该块() {
    // error 帧在 2xx 之后到达：表现为帧解码失败，可重试一次。
    let error_frame = json!({"error": {"message": "boom", "type": "server_error"}});
    let reply = Reply::Sse(
        vec![
            (Duration::ZERO, chunk(Some("半截"), None, None)),
            (Duration::ZERO, error_frame),
        ],
        true,
    );
    let model = FakeModel::start_by_content(vec![("block\n".to_string(), reply)]);
    let dir = TempDir::new("t21-error-frame");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(model.count(), 2, "流错误重试一次");
    assert_eq!(text(&run.stdout), "block\n");
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
}

#[test]
fn 写一半断开重试一次后回退() {
    // 帧发出后直接断开：无 finish_reason、无 [DONE]，按可重试失败。
    let cut = Reply::Sse(vec![(Duration::ZERO, chunk(Some("cut"), None, None))], false);
    let model = FakeModel::start_by_content(vec![("block\n".to_string(), cut)]);
    let dir = TempDir::new("t21-cut");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(model.count(), 2);
    assert_eq!(text(&run.stdout), "block\n");
}

#[test]
fn finish_reason_length_与空内容都重试一次后回退() {
    for (tag, reply) in [
        ("t21-length", sse_reply("cut\n", Some("length"), None)),
        // 空 content：流正常收齐但内容为空，按可重试失败。
        ("t21-empty", sse_reply("   \n", Some("stop"), None)),
        // finish_reason 缺失：判定不依赖 [DONE]。
        ("t21-no-finish", sse_reply("ok\n", None, None)),
    ] {
        let model = FakeModel::start_by_content(vec![("block\n".to_string(), reply)]);
        let dir = TempDir::new(tag);
        dir.write_config(&model_config(&model, ""));
        let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
        assert_eq!(model.count(), 2, "{tag}");
        assert_eq!(text(&run.stdout), "block\n", "{tag}");
    }
}

#[test]
fn 多块并发乱序完成_输出恒按块序() {
    // 首块慢 600ms，后续块立即完成：直播块轮到前后续块静默累积。
    let script: Vec<(String, Reply)> = ["a\n", "b\n", "c\n"]
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let delay = if index == 0 { 600 } else { 0 };
            let reply = Reply::Sse(
                vec![
                    (Duration::from_millis(delay), chunk(Some(&format!("t{index}\n")), None, None)),
                    (Duration::ZERO, chunk(None, Some("stop"), None)),
                ],
                true,
            );
            ((*name).to_string(), reply)
        })
        .collect();
    let model = FakeModel::start_by_content(script);
    let dir = TempDir::new("t21-out-of-order");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n\nc\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "t0\n\nt1\n\nt2\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}
