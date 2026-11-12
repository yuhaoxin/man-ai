//! 后端请求形状、失败分类与重试（§7.1、§7.7、§10.3–§10.5）。

mod common;

use std::time::{Duration, Instant};

use common::{
    DisconnectServer, FakeModel, Reply, Run, TempDir, completion, error_body, model_config,
    run_pipe, text,
};
use man_ai::language::Language;
use man_ai::prompt;
use serde_json::json;

const PROMPT_TAG: &str = "zh-CN";

fn lang() -> Language {
    Language::parse_cli(PROMPT_TAG).unwrap()
}

fn translate(dir: &TempDir, stdin: &[u8]) -> Run {
    run_pipe(dir, &["--translate", "--lang", PROMPT_TAG], stdin, &[])
}

#[test]
fn 请求体字段固定且系统提示逐字节() {
    let model = FakeModel::start(vec![Reply::Ok(completion("译文\n", "stop"))]);
    let dir = TempDir::new("request-shape");
    dir.write_config(&model_config(&model, ""));
    let input = "NAME\n     ls - list\n";
    let run = translate(&dir, input.as_bytes());
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, "译文\n".as_bytes());
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));

    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    // endpoint 只去掉尾部 `/`，不补 `/v1`。
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk-test"));

    let body = request.body.as_object().unwrap();
    let mut keys: Vec<&str> = body.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["messages", "model", "stream", "temperature"]);
    assert_eq!(request.body["model"], "test-model");
    assert_eq!(request.body["temperature"].as_f64(), Some(0.0));
    assert_eq!(request.body["stream"], json!(false));

    let messages = request.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], prompt::page(&lang()));
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], input);
}

#[test]
fn max_tokens_仅在配置时出现() {
    let model = FakeModel::start(vec![Reply::Ok(completion("ok\n", "stop"))]);
    let dir = TempDir::new("max-tokens");
    dir.write_config(&model_config(&model, "max_tokens = 4096\n"));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.code, 0);
    let requests = model.requests();
    assert_eq!(requests[0].body["max_tokens"], json!(4096));
}

#[test]
fn 每块一次请求且本地端点用裸_bearer() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("bare-bearer");
    // 本地端点不带 api_key。
    // 脚本化应答按请求到达顺序对应块，只有串行时这个对应关系才确定。
    dir.write_config(&format!(
        "endpoint = \"{}/\"\nmodel = \"test-model\"\nconcurrency = 1\n",
        model.endpoint()
    ));
    let run = translate(&dir, b"a\n\nb\n");
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, "甲\n\n乙\n".as_bytes());
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    // 客户端发的是 `Bearer `（空密钥），HTTP 解析器会把行尾空白裁掉。
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer"));
    assert_eq!(requests[0].body["messages"][1]["content"], "a\n");
    assert_eq!(requests[1].body["messages"][1]["content"], "b\n");
}

#[test]
fn 原样回显按采用处理() {
    let page = "NAME\n     ls - list\n";
    let model = FakeModel::start(vec![Reply::Ok(completion(page, "stop"))]);
    let dir = TempDir::new("echo");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, page.as_bytes());
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, page.as_bytes());
    assert_eq!(model.count(), 1);
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}

#[test]
fn http_400_不重试且回退原文() {
    let model = FakeModel::start(vec![Reply::Status(
        400,
        error_body("invalid_request_error", "invalid_request_error"),
    )]);
    let dir = TempDir::new("http-400");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(model.count(), 1);
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains(
            "man-ai: backend returned HTTP 400; error.code=invalid_request_error; error.type=invalid_request_error"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
    assert!(!stderr.contains("scripted failure"), "{stderr}");
}

#[test]
fn http_401_给出检查密钥的文案且不重试() {
    let model = FakeModel::start(vec![Reply::Status(
        401,
        error_body("invalid_api_key", "invalid_request_error"),
    )]);
    let dir = TempDir::new("http-401");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(model.count(), 1);
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains("man-ai: backend returned HTTP 401; check api_key / api_key_env"),
        "{stderr}"
    );
}

#[test]
fn http_429_重试一次后成功() {
    let model = FakeModel::start(vec![
        Reply::Status(429, error_body("rate_limit_exceeded", "requests")),
        Reply::Ok(completion("译\n", "stop")),
    ]);
    let dir = TempDir::new("http-429");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, "译\n".as_bytes());
    assert_eq!(model.count(), 2);
}

#[test]
fn http_429_重试一次后仍失败则回退该块() {
    let model = FakeModel::start(vec![
        Reply::Status(429, error_body("rate_limit_exceeded", "requests")),
        Reply::Status(429, error_body("rate_limit_exceeded", "requests")),
    ]);
    let dir = TempDir::new("http-429-fail");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(model.count(), 2, "429 只重试一次");
    let stderr = text(&run.stderr);
    assert_eq!(
        stderr.matches("backend returned HTTP 429").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
}

#[test]
fn http_500_重试一次后仍失败则回退该块() {
    let model = FakeModel::start(vec![
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
    ]);
    let dir = TempDir::new("http-500");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(model.count(), 2);
    let stderr = text(&run.stderr);
    // 同一诊断只写一次。
    assert_eq!(
        stderr.matches("backend returned HTTP 500").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
}

#[test]
fn 超时不重试() {
    let model = FakeModel::start(vec![Reply::Delay(
        Duration::from_secs(2),
        completion("late\n", "stop"),
    )]);
    let dir = TempDir::new("timeout");
    dir.write_config(&model_config(&model, "timeout_seconds = 0.3\n"));
    let started = Instant::now();
    let run = translate(&dir, b"block\n");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(model.count(), 1);
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains("man-ai: translation failed (timeout), showing the original below"),
        "{stderr}"
    );
}

#[test]
fn 连接中断重试一次() {
    let server = DisconnectServer::start();
    let dir = TempDir::new("disconnect");
    dir.write_config(&format!(
        "endpoint = \"{}/\"\nmodel = \"test-model\"\napi_key = \"sk-test\"\n",
        server.endpoint()
    ));
    let run = translate(&dir, b"block\n");
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(server.connections(), 2);
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains("man-ai: translation failed (backend error), showing the original below"),
        "{stderr}"
    );
}

#[test]
fn 非_json_空_choices_与截断都重试一次() {
    for (tag, replies) in [
        ("not-json", vec![Reply::Raw(200, "not json".into())]),
        ("empty-choices", vec![Reply::Ok(json!({"choices": []}))]),
        ("length", vec![Reply::Ok(completion("cut", "length"))]),
    ] {
        let model = FakeModel::start(vec![replies[0].clone(), replies[0].clone()]);
        let dir = TempDir::new(tag);
        dir.write_config(&model_config(&model, ""));
        let run = translate(&dir, b"block\n");
        assert_eq!(run.stdout, b"block\n", "{tag}");
        assert_eq!(model.count(), 2, "{tag}");
    }
}

#[test]
fn 未配置后端时不发请求() {
    let dir = TempDir::new("unconfigured");
    dir.write_config("endpoint = \"http://127.0.0.1:1/v1\"\n");
    let run = translate(&dir, b"block\n");
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, b"block\n");
    assert_eq!(
        text(&run.stderr),
        "man-ai: backend not configured (missing model); not translating\n\
         man-ai: translation failed (not configured), showing the original below\n"
    );
}

#[test]
fn 非_tty_没有_translate_时原文直通() {
    let model = FakeModel::start(vec![Reply::Ok(completion("译\n", "stop"))]);
    let dir = TempDir::new("no-translate-flag");
    dir.write_config(&model_config(&model, ""));
    let input = b"a\n\nb\n";
    let run = run_pipe(&dir, &["--lang", PROMPT_TAG], input, &[]);
    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, input);
    assert_eq!(model.count(), 0);
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}

#[test]
fn 非_tty_带_translate_才发请求() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("translate-flag");
    dir.write_config(&model_config(&model, ""));
    let run = translate(&dir, b"a\n\nb\n");
    assert_eq!(run.stdout, "甲\n\n乙\n".as_bytes());
    assert_eq!(model.count(), 2);
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
}
