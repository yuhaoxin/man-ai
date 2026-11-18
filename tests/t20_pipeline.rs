//! 分页器模式的 TTY 页首行/尾行、部分回退与确定性失败（§8.3、§8.5）。

mod common;

use common::{
    FakeModel, Reply, TempDir, completion, error_body, model_config, run_pager_tty, run_pipe, text,
};

const LANG: &str = "zh-CN";
const HEADER: &str = "── man-ai: translation (Simplified Chinese) ──\n\n";
const PROGRESS: &str = "man-ai: translating (Simplified Chinese)…\n";

#[test]
fn 全部采用时页首行与尾行() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("tty-adopted");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        text(&run.output),
        format!("{HEADER}甲\n\n乙\n\n── man-ai: translated (Simplified Chinese) ──\n")
    );
    assert_eq!(model.count(), 2);
    // §8.4：首个块写出、分页器尚未启动时提示一行。
    assert_eq!(run.stderr, PROGRESS);
}

#[test]
fn 可重试失败回退单块并报部分翻译() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
    ]);
    let dir = TempDir::new("tty-partial");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}甲\n\nb\n\n── man-ai: partially translated (1/2 blocks, backend error) ──\n"
        )
    );
    assert_eq!(model.count(), 3);
}

#[test]
fn 确定性失败停止提交后续块() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Status(401, error_body("invalid_api_key", "invalid_request_error")),
    ]);
    let dir = TempDir::new("tty-deterministic");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n\nc\n", &[]);
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}甲\n\nb\n\nc\n\n── man-ai: translation failed (backend error), remaining blocks left untranslated ──\n"
        )
    );
    // 第三块没有再发请求。
    assert_eq!(model.count(), 2);
    assert!(
        run.stderr
            .contains("man-ai: backend returned HTTP 401; check api_key / api_key_env"),
        "{}",
        run.stderr
    );
}

#[test]
fn 每进程最多一条失败诊断() {
    let model = FakeModel::start(vec![
        Reply::Status(429, error_body("rate_limit_exceeded", "requests")),
        Reply::Status(429, error_body("rate_limit_exceeded", "requests")),
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Status(500, error_body("server_error", "api_error")),
    ]);
    let dir = TempDir::new("diagnostic-once");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(model.count(), 4, "两块各重试一次");
    let stderr = text(&run.stderr);
    assert_eq!(
        stderr.matches("backend returned HTTP").count(),
        1,
        "只保留第一个失败的诊断：{stderr}"
    );
    assert!(stderr.contains("HTTP 429"), "{stderr}");
}

#[test]
fn 未配置后端全部原文并写尾行() {
    let dir = TempDir::new("tty-unconfigured");
    dir.write_config("endpoint = \"http://127.0.0.1:1/v1\"\n");
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(run.code, 0);
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}a\n\nb\n\n── man-ai: translation failed (not configured), remaining blocks left untranslated ──\n"
        )
    );
    assert_eq!(
        run.stderr,
        format!("man-ai: backend not configured (missing model); not translating\n{PROGRESS}")
    );
}

#[test]
fn 空_stdin_与只有空行不发请求也没有页首尾行() {
    let model = FakeModel::start(vec![]);
    let dir = TempDir::new("tty-empty");
    dir.write_config(&model_config(&model, ""));
    for input in [&b""[..], b"\n\n"] {
        let run = run_pager_tty(&dir, &["--lang", LANG], input, &[]);
        assert_eq!(run.output, input);
        assert!(run.stderr.is_empty(), "{}", run.stderr);
    }
    assert_eq!(model.count(), 0);
}

#[test]
fn 非_utf8_块有损解码后照常翻译() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("乙\n", "stop")),
    ]);
    let dir = TempDir::new("tty-non-utf8");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"ok\n\n\xff\xfe\n", &[]);
    assert_eq!(model.count(), 2);
    // 第二块按 U+FFFD 有损解码后送模型，也计入采用。
    let requests = model.requests();
    assert_eq!(requests[1].body["messages"][1]["content"], "\u{fffd}\u{fffd}\n");
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}甲\n\n乙\n\n── man-ai: translated (Simplified Chinese) ──\n"
        )
    );
}

#[test]
fn 空白响应重试一次后回退该块且不半截写入() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("   \n", "stop")),
        Reply::Ok(completion("   \n", "stop")),
    ]);
    let dir = TempDir::new("tty-blank");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n", &[]);
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}甲\n\nb\n\n── man-ai: partially translated (1/2 blocks, backend error) ──\n"
        )
    );
    assert_eq!(model.count(), 3);
}

#[test]
fn 畸形但_2xx_的响应照常采用() {
    // §7.4：不做内容检查，控制字符不构成失败；已知风险，渲染侧统一处理。
    let bad = "\x1b[1m字面 ESC[1m 与退格 \x08\n";
    let model = FakeModel::start(vec![Reply::Ok(completion(bad, "stop"))]);
    let dir = TempDir::new("tty-malformed-2xx");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"block\n", &[]);
    assert_eq!(model.count(), 1);
    assert_eq!(
        text(&run.output),
        format!("{HEADER}{bad}\n── man-ai: translated (Simplified Chinese) ──\n")
    );
}

#[test]
fn 原样回显在_tty_下计入采用() {
    let page = "NAME\n     ls - list\n";
    let model = FakeModel::start(vec![Reply::Ok(completion(page, "stop"))]);
    let dir = TempDir::new("tty-echo");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], page.as_bytes(), &[]);
    assert_eq!(
        text(&run.output),
        format!("{HEADER}{page}\n── man-ai: translated (Simplified Chinese) ──\n")
    );
}

#[test]
fn 超时只回退该块并报部分翻译() {
    // 两块都超时（不重试），尾行原因 timeout。
    let model = FakeModel::start(vec![
        Reply::Delay(std::time::Duration::from_secs(2), completion("a", "stop")),
        Reply::Delay(std::time::Duration::from_secs(2), completion("b", "stop")),
    ]);
    let dir = TempDir::new("tty-timeout");
    dir.write_config(&model_config(&model, "timeout_seconds = 0.2\n"));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"a\n\nb\n", &[]);
    let output = text(&run.output);
    assert_eq!(model.count(), 2);
    assert!(
        output.ends_with("── man-ai: partially translated (0/2 blocks, timeout) ──\n"),
        "{output}"
    );
    assert!(!run.stderr.contains("translation failed"), "{}", run.stderr);
}
