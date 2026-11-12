//! #20：分页器模式的 TTY 页首行/尾行、部分回退与确定性失败（§8.3、§8.5）。

mod common;

use common::{
    FakeModel, Reply, TempDir, completion, error_body, model_config, run_pager_tty, text,
};

const LANG: &str = "zh-CN";
const HEADER: &str = "── man-ai: translation (Simplified Chinese) ──\n\n";

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
    assert!(run.stderr.is_empty(), "{}", run.stderr);
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
        "man-ai: backend not configured (missing model); not translating\n"
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
fn 非_utf8_块不送模型且原样输出() {
    let model = FakeModel::start(vec![Reply::Ok(completion("译\n", "stop"))]);
    let dir = TempDir::new("tty-non-utf8");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(&dir, &["--lang", LANG], b"ok\n\n\xff\xfe\n", &[]);
    assert_eq!(model.count(), 1);
    let mut expected = Vec::new();
    expected.extend_from_slice(HEADER.as_bytes());
    expected.extend_from_slice("译\n".as_bytes());
    expected.extend_from_slice(b"\n\xff\xfe\n\n");
    expected.extend_from_slice("── man-ai: translated (Simplified Chinese) ──\n".as_bytes());
    assert_eq!(run.output, expected);
}

#[test]
fn 结构非法时回退该块且不半截写入() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("甲\n", "stop")),
        Reply::Ok(completion("\x08bad", "stop")),
        Reply::Ok(completion("\x08bad", "stop")),
    ]);
    let dir = TempDir::new("tty-invalid");
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
fn 字面转义与非法_sgr_也回退() {
    for (tag, bad) in [
        ("literal", "text ESC[1m more"),
        ("sgr", "\x1b[1mnever closed"),
    ] {
        let model = FakeModel::start(vec![
            Reply::Ok(completion(bad, "stop")),
            Reply::Ok(completion(bad, "stop")),
        ]);
        let dir = TempDir::new(tag);
        dir.write_config(&model_config(&model, ""));
        let run = run_pager_tty(&dir, &["--lang", LANG], b"block\n", &[]);
        assert_eq!(
            text(&run.output),
            format!(
                "{HEADER}block\n\n── man-ai: partially translated (0/1 blocks, backend error) ──\n"
            ),
            "{tag}"
        );
        assert_eq!(model.count(), 2, "{tag}");
    }
}

#[test]
fn 样式丢失重试一次后接受() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("没有样式\n", "stop")),
        Reply::Ok(completion("有样式\n", "stop")),
    ]);
    let dir = TempDir::new("style-lost");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(
        &dir,
        &["--lang", LANG],
        "\x1b[1mbold\x1b[0m\n".as_bytes(),
        &[],
    );
    assert_eq!(model.count(), 2);
    assert_eq!(
        text(&run.output),
        format!("{HEADER}有样式\n\n── man-ai: translated (Simplified Chinese) ──\n")
    );
}

#[test]
fn 样式重试后仍缺失也接受() {
    let model = FakeModel::start(vec![
        Reply::Ok(completion("一\n", "stop")),
        Reply::Ok(completion("二\n", "stop")),
    ]);
    let dir = TempDir::new("style-lost-accept");
    dir.write_config(&model_config(&model, ""));
    let run = run_pager_tty(
        &dir,
        &["--lang", LANG],
        "\x1b[1mbold\x1b[0m\n".as_bytes(),
        &[],
    );
    assert_eq!(model.count(), 2);
    assert_eq!(
        text(&run.output),
        format!("{HEADER}二\n\n── man-ai: translated (Simplified Chinese) ──\n")
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
