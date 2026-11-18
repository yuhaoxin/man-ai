//! 分页器模式的翻译管线（§8.5）。
//!
//! 首尾行与按序渲染在阅读器单元层（src/reader.rs）断言；这里用非 TTY 的
//! `--translate` 按字节断言块内容、回退、诊断与请求形状。

mod common;

use common::{FakeModel, Reply, TempDir, completion, error_body, model_config, run_pipe, text};

const LANG: &str = "zh-CN";

#[test]
fn 全部采用时按序写出译文() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("pipe-adopted");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "甲\n\n乙\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
    assert_eq!(model.count(), 2);
}

#[test]
fn 可重试失败回退单块() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
    ]);
    let dir = TempDir::new("pipe-partial");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    // 回退块是预转换原文。
    assert_eq!(text(&run.stdout), "甲\n\nb\n");
    assert!(
        text(&run.stderr).contains("backend returned HTTP 500"),
        "{}",
        text(&run.stderr)
    );
    assert_eq!(model.count(), 3);
}

#[test]
fn 确定性失败停止提交后续块() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Status(401, error_body("invalid_api_key", "invalid_request_error")),
    ]);
    let dir = TempDir::new("pipe-deterministic");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(
        &dir,
        &["--translate", "--lang", LANG],
        b"a\n\nb\n\nc\n",
        &[],
    );
    // 停止提交后其余块写预转换原文，仍按序完整写出。
    assert_eq!(text(&run.stdout), "甲\n\nb\n\nc\n");
    // 第三块没有再发请求。
    assert_eq!(model.count(), 2);
    assert!(
        text(&run.stderr)
            .contains("man-ai: backend returned HTTP 401; check api_key / api_key_env"),
        "{}",
        text(&run.stderr)
    );
}

#[test]
fn 未配置后端全部原文并提示整页失败() {
    let dir = TempDir::new("pipe-unconfigured");
    dir.write_config("endpoint = \"http://127.0.0.1:1/v1\"\n");
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "a\n\nb\n");
    let stderr = text(&run.stderr);
    assert!(
        stderr.contains("man-ai: backend not configured (missing model); not translating\n"),
        "{stderr}"
    );
    assert!(
        stderr
            .contains("man-ai: translation failed (not configured), showing the original below\n"),
        "{stderr}"
    );
}

#[test]
fn 空_stdin_与只有空行不发请求原样输出() {
    let model = FakeModel::start(vec![]);
    let dir = TempDir::new("pipe-empty");
    dir.write_config(&model_config(&model, ""));
    for input in [&b""[..], b"\n\n"] {
        let run = run_pipe(&dir, &["--translate", "--lang", LANG], input, &[]);
        assert_eq!(run.stdout, input);
        assert!(run.stderr.is_empty(), "{}", text(&run.stderr));
    }
    assert_eq!(model.count(), 0);
}

#[test]
fn 非_utf8_块有损解码后照常翻译() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("pipe-non-utf8");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(
        &dir,
        &["--translate", "--lang", LANG],
        b"ok\n\n\xff\xfe\n",
        &[],
    );
    assert_eq!(model.count(), 2);
    // 第二块按 U+FFFD 有损解码后送模型，也计入采用。
    let requests = model.requests();
    assert_eq!(
        requests[1].body["messages"][1]["content"],
        "\u{fffd}\u{fffd}\n"
    );
    assert_eq!(text(&run.stdout), "甲\n\n乙\n");
}

#[test]
fn 空白响应重试一次后回退该块且不半截写入() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("   \n", "stop")),
        Reply::Ok(completion("   \n", "stop")),
    ]);
    let dir = TempDir::new("pipe-blank");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(text(&run.stdout), "甲\n\nb\n");
    assert_eq!(model.count(), 3);
}

#[test]
fn 畸形但_2xx_的响应照常采用() {
    // §7.4：不做内容检查，控制字符不构成失败；已知风险，渲染侧统一处理。
    let bad = "\x1b[1m字面 ESC[1m 与退格 \x08\n";
    let model = FakeModel::start(vec![Reply::Ok(completion(bad, "stop"))]);
    let dir = TempDir::new("pipe-malformed-2xx");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"block\n", &[]);
    assert_eq!(model.count(), 1);
    assert_eq!(text(&run.stdout), bad);
}

#[test]
fn 原样回显计入采用() {
    let page = "NAME\n     ls - list\n";
    let model = FakeModel::start(vec![Reply::Ok(completion(page, "stop"))]);
    let dir = TempDir::new("pipe-echo");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], page.as_bytes(), &[]);
    assert_eq!(text(&run.stdout), page);
}

#[test]
fn 超时只回退该块不重试() {
    // 两块都超时（不重试），整页回退但退出码不变。
    let model = FakeModel::start(vec![
        Reply::Delay(std::time::Duration::from_secs(2), completion("a", "stop")),
        Reply::Delay(std::time::Duration::from_secs(2), completion("b", "stop")),
    ]);
    let dir = TempDir::new("pipe-timeout");
    dir.write_config(&model_config(&model, "timeout_seconds = 0.2\n"));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(run.code, 0, "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "a\n\nb\n");
    assert_eq!(model.count(), 2);
    assert!(
        text(&run.stderr)
            .contains("man-ai: translation failed (timeout), showing the original below"),
        "{}",
        text(&run.stderr)
    );
}
