//! 阅读器的 PTY 进程级契约（§5.4、§8、§13.3）。
//!
//! 接管/恢复终端、`q`、Ctrl-C 两阶段、SIGTERM/SIGHUP 按信号死、Ctrl-Z 忽略、
//! 阅读器初始化失败直写原文、不翻译 TTY 页面进阅读器、直播块增量上屏。

mod common;

use std::time::Duration;

use common::{
    FakeModel, Reply, TempDir, chunk, compact, completion, model_config_with, plain,
    run_reader_driven, run_reader_signaled, run_reader_tty, wait_seen,
};
use serde_json::json;

const LANG: &str = "zh-CN";
/// 首块前占位行（空格在帧 diff 里被跳过，断言去空格）。
const PLACEHOLDER: &str = "Translating…(waitingforthefirstblock)";
/// 进/出备用屏幕的序列（§8.1 接管与恢复）。
const ENTER_ALT: &str = "\x1b[?1049h";
const LEAVE_ALT: &str = "\x1b[?1049l";

/// 两个块按内容回声。
fn echo_two() -> Vec<(String, Reply)> {
    vec![
        ("a\n".to_string(), Reply::Ok(completion("t1\n", "stop"))),
        ("b\n".to_string(), Reply::Ok(completion("t2\n", "stop"))),
    ]
}

#[test]
fn 接管终端_占位行_按序正文_q_退出零() {
    let model = FakeModel::start_by_content(echo_two());
    let dir = TempDir::new("t26-basic");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_tty(
        &dir,
        &["--lang", LANG],
        b"a\n\nb\n",
        &[],
        "── man-ai: translation (Simplified Chinese) ──",
        &["q"],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    // 接管与恢复：进/出备用屏幕都在原始流里。
    let raw = String::from_utf8_lossy(&run.raw);
    assert!(raw.contains(ENTER_ALT), "未接管终端");
    assert!(raw.contains(LEAVE_ALT), "未恢复终端");
    // 页首行、按序正文、尾行、状态行；空格被帧 diff 跳过，比较去空格。
    let text = compact(&run.text());
    assert!(
        text.contains("──man-ai:translation(SimplifiedChinese)──"),
        "{text}"
    );
    assert!(text.contains("t1"), "{text}");
    assert!(text.contains("t2"), "{text}");
    assert!(
        text.find("t1").unwrap() < text.find("t2").unwrap(),
        "{text}"
    );
    assert!(
        text.contains("──man-ai:translated(SimplifiedChinese)──"),
        "{text}"
    );
    assert!(text.contains("elapsed"), "{text}");
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

#[test]
fn 首块前显示占位行() {
    // 首块 2 秒才回：占位行必然先出现。
    let model = FakeModel::start_by_content(vec![
        (
            "a\n".to_string(),
            Reply::Delay(Duration::from_secs(2), completion("t1\n", "stop")),
        ),
        ("b\n".to_string(), Reply::Ok(completion("t2\n", "stop"))),
    ]);
    let dir = TempDir::new("t26-placeholder");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_driven(
        &dir,
        &["--lang", LANG],
        b"a\n\nb\n",
        &[],
        "",
        move |_pid, writer, seen| {
            wait_seen(seen, PLACEHOLDER, Duration::from_secs(10));
            // 占位行确认后等收尾。
            wait_seen(
                seen,
                "──man-ai:translated(SimplifiedChinese)──",
                Duration::from_secs(30),
            );
            let _ = writer.write_all(b"q");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    assert!(text.contains(PLACEHOLDER), "{text}");
    assert!(text.contains("t1"), "{text}");
}

#[test]
fn 翻译期间第一次_ctrl_c_取消_结束后等同_q() {
    // 两块都慢：Ctrl-C 时必然在途。
    let model = FakeModel::start_by_content(vec![
        (
            "a\n".to_string(),
            Reply::Delay(Duration::from_secs(10), completion("t1\n", "stop")),
        ),
        (
            "b\n".to_string(),
            Reply::Delay(Duration::from_secs(10), completion("t2\n", "stop")),
        ),
    ]);
    let dir = TempDir::new("t26-ctrl-c");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_driven(
        &dir,
        &["--lang", LANG],
        b"a\n\nb\n",
        &[],
        PLACEHOLDER,
        move |_pid, writer, seen| {
            // 翻译期间第一次 Ctrl-C：取消剩余翻译。
            let _ = writer.write_all(b"\x03");
            // 尾行报 interrupted；未完成块写预转换原文。
            wait_seen(
                seen,
                "──man-ai:translationfailed(interrupted),remainingblocksleftuntranslated──",
                Duration::from_secs(30),
            );
            // 阅读器留在屏幕上：等第二次 Ctrl-C 才退出。
            let _ = writer.write_all(b"\x03");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    assert!(
        text.contains("──man-ai:translationfailed(interrupted),remainingblocksleftuntranslated──"),
        "{text}"
    );
    // 未完成块按预转换原文写出。
    assert!(text.contains("a"), "{text}");
    assert!(text.contains("b"), "{text}");
}

#[test]
fn ctrl_z_被忽略() {
    let model = FakeModel::start_by_content(vec![
        (
            "a\n".to_string(),
            Reply::Delay(Duration::from_secs(1), completion("t1\n", "stop")),
        ),
        ("b\n".to_string(), Reply::Ok(completion("t2\n", "stop"))),
    ]);
    let dir = TempDir::new("t26-ctrl-z");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_driven(
        &dir,
        &["--lang", LANG],
        b"a\n\nb\n",
        &[],
        PLACEHOLDER,
        move |_pid, writer, seen| {
            // raw 模式下 Ctrl-Z 是普通按键：忽略后翻译照常完成。
            let _ = writer.write_all(b"\x1a");
            wait_seen(
                seen,
                "──man-ai:translated(SimplifiedChinese)──",
                Duration::from_secs(30),
            );
            let _ = writer.write_all(b"q");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    assert!(text.contains("t1"), "{text}");
    assert!(text.contains("t2"), "{text}");
}

#[test]
fn sigterm_恢复终端后按信号死() {
    let model = FakeModel::start_by_content(vec![(
        "a\n".to_string(),
        Reply::Delay(Duration::from_secs(30), completion("t1\n", "stop")),
    )]);
    let dir = TempDir::new("t26-term");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_signaled(&dir, &["--lang", LANG], b"a\n", &[], PLACEHOLDER, "TERM");
    assert_eq!(run.code, 128 + 15, "{}", run.stderr);
    // 终端先恢复再死：备用屏幕退出序列已写出。
    let raw = String::from_utf8_lossy(&run.raw);
    assert!(raw.contains(LEAVE_ALT), "SIGTERM 未恢复终端");
}

#[test]
fn sighup_恢复终端后按信号死() {
    let model = FakeModel::start_by_content(vec![(
        "a\n".to_string(),
        Reply::Delay(Duration::from_secs(30), completion("t1\n", "stop")),
    )]);
    let dir = TempDir::new("t26-hup");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_signaled(&dir, &["--lang", LANG], b"a\n", &[], PLACEHOLDER, "HUP");
    assert_eq!(run.code, 128 + 1, "{}", run.stderr);
    let raw = String::from_utf8_lossy(&run.raw);
    assert!(raw.contains(LEAVE_ALT), "SIGHUP 未恢复终端");
}

#[test]
fn 初始化失败_警告并直写预转换原文() {
    let model = FakeModel::start_by_content(echo_two());
    let dir = TempDir::new("t26-init-fail");
    dir.write_config(&model_config_with(&model, 4, ""));
    // TERM 置空：阅读器初始化失败（§5.4）。
    let run = run_reader_tty(
        &dir,
        &["--lang", LANG],
        b"a\n\nb\n",
        &[("TERM", String::new())],
        "",
        &["q"],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("man-ai: terminal unavailable (TERM is not set); showing the original"),
        "{}",
        run.stderr
    );
    // 跳过翻译：预转换原文直写 stdout，不进备用屏幕。
    let raw = String::from_utf8_lossy(&run.raw);
    assert!(!raw.contains(ENTER_ALT), "不应接管终端");
    let text = compact(&plain(&run.raw));
    assert!(text.contains("a"), "{text}");
    assert!(text.contains("b"), "{text}");
    assert_eq!(model.count(), 0, "初始化失败不应发请求");
}

#[test]
fn 不翻译的_tty_页面进阅读器显示预转换原文() {
    let model = FakeModel::start_by_content(Vec::new());
    let dir = TempDir::new("t26-no-translate");
    dir.write_config(&model_config_with(&model, 4, ""));
    // SGR 粗体与退格叠加：预转换后是 Markdown 强调。
    let input = b"\x1b[1mls\x1b[0m - list\n\nplain\n";
    let run = run_reader_tty(
        &dir,
        &["--lang", LANG, "--no-translate"],
        input,
        &[],
        "ls - list",
        &["q"],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    // 无页首行与尾行；正文是预转换原文（粗体标记渲染成样式，不显示 `**`）。
    assert!(!text.contains("──man-ai:translation"), "{text}");
    assert!(!text.contains("──man-ai:translated"), "{text}");
    assert!(text.contains("ls-list"), "{text}");
    assert!(text.contains("plain"), "{text}");
    // 状态行只有 elapsed。
    assert!(text.contains("elapsed"), "{text}");
    // 不发请求、不查缓存。
    assert_eq!(model.count(), 0);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

#[test]
fn 直播块增量上屏_首帧先于流收齐() {
    // 首帧后停 600ms 再收尾：增量必然先于尾行出现在屏幕上。
    let reply = Reply::Sse(
        vec![
            (Duration::ZERO, chunk(Some("PART"), None, None)),
            (
                Duration::from_millis(600),
                chunk(Some("IAL"), None, None),
            ),
            (Duration::ZERO, chunk(None, Some("stop"), None)),
        ],
        true,
    );
    let model = FakeModel::start(vec![reply]);
    let dir = TempDir::new("t26-live");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_driven(
        &dir,
        &["--lang", LANG],
        b"block\n",
        &[],
        "",
        move |_pid, writer, seen| {
            // 直播增量：流尚未收齐，首帧内容已在屏幕上。
            wait_seen(seen, "PART", Duration::from_secs(10));
            wait_seen(
                seen,
                "──man-ai:translated(SimplifiedChinese)──",
                Duration::from_secs(30),
            );
            let _ = writer.write_all(b"q");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    assert!(text.contains("PARTIAL"), "{text}");
}

#[test]
fn 直播块失败整块替换并挂_fallback_注记() {
    // 两次尝试都在流中途遇到 error 帧：重试一次后整块替换为预转换原文。
    let error_frame = json!({"error": {"message": "boom", "type": "server_error"}});
    let reply = Reply::Sse(
        vec![
            (Duration::ZERO, chunk(Some("半截"), None, None)),
            (Duration::ZERO, error_frame),
        ],
        true,
    );
    let model = FakeModel::start_by_content(vec![("原文块\n".to_string(), reply)]);
    let dir = TempDir::new("t26-fallback");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_reader_driven(
        &dir,
        &["--lang", LANG],
        "原文块\n".as_bytes(),
        &[],
        "",
        move |_pid, writer, seen| {
            wait_seen(
                seen,
                "fallback:backenderror",
                Duration::from_secs(30),
            );
            // 整块替换后的最终形态：部分回退的尾行。
            wait_seen(
                seen,
                "──man-ai:partiallytranslated(0/1blocks,backenderror)──",
                Duration::from_secs(30),
            );
            let _ = writer.write_all(b"q");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let text = compact(&run.text());
    // 整块替换：直播的半截译文不在屏幕上，原文在。
    assert!(text.contains("原文块"), "{text}");
    assert!(!text.contains("半截"), "{text}");
    assert!(text.contains("fallback:backenderror"), "{text}");
}
