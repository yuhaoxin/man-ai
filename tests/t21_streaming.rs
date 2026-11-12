//! #21：首屏流式、每块 flush、进度行、EPIPE、两阶段 Ctrl-C（§8.1–§8.5、§5.6）。

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{
    FakeModel, Reply, TempDir, completion, error_body, model_config_with, pager_log,
    quitting_pager, recording_pager, run_pager_tty, run_pager_tty_with_pid, run_pipe, send_sigint,
    text,
};

const LANG: &str = "zh-CN";
const HEADER: &str = "── man-ai: translation (Simplified Chinese) ──\n\n";
const PROGRESS: &str = "man-ai: translating (Simplified Chinese)…\n";

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

/// 分页器每行加换行回显，所以期望输出就是逐行拼回去。
fn lines_to_output(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
}

/// 等待文件出现；用于「分页器在慢块完成前已启动」的断言。
fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

#[test]
fn 首屏在全部块完成前启动且按序_flush() {
    let names: Vec<String> = (0..30).map(|index| format!("b{index}")).collect();
    let mut script = echo_script(&names);
    let slow = names.last().unwrap().clone();
    script.retain(|(key, _)| *key != format!("{slow}\n"));
    let dir = TempDir::new("t21-first-screen");
    let pager = recording_pager(&dir, "screen");
    let start = dir.file("screen.start");
    // 慢块等到分页器标记文件出现才应答：用同步信号代替固定计时窗口，
    // 高负载下不会误判，正确实现下也几乎不增加测试时长。
    script.push((
        format!("{slow}\n"),
        Reply::AwaitFile(
            start.clone(),
            Duration::from_secs(30),
            completion("t29\n", "stop"),
        ),
    ));
    let model = FakeModel::start_by_content(script);
    dir.write_config(&model_config_with(&model, 4, ""));
    let seen = Arc::new(AtomicBool::new(false));
    let flag = seen.clone();
    let run = run_pager_tty_with_pid(
        &dir,
        &["--lang", LANG],
        page(&names).as_bytes(),
        &[("MAN_AI_PAGER", pager.to_str().unwrap())],
        move |_pid| {
            if wait_for(&start, Duration::from_secs(15)) {
                flag.store(true, Ordering::SeqCst);
            }
        },
    );
    assert!(
        seen.load(Ordering::SeqCst),
        "分页器应在慢块完成前启动（首屏流式）"
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    // 分页器边读边记录：收到的行序 = 页首行 + 按块序正文 + 尾行。
    let mut expected: Vec<String> = vec![
        "── man-ai: translation (Simplified Chinese) ──".into(),
        String::new(),
    ];
    for (index, _) in names.iter().enumerate() {
        if index > 0 {
            expected.push(String::new());
        }
        expected.push(format!("t{index}"));
    }
    expected.push(String::new());
    expected.push("── man-ai: translated (Simplified Chinese) ──".into());
    assert_eq!(pager_log(&dir, "screen"), expected);
    assert_eq!(text(&run.output), lines_to_output(&expected));
    assert_eq!(run.stderr, PROGRESS);
}

#[test]
fn 进度行只写一次且分页器启动后不写_stderr() {
    let names: Vec<String> = (0..30).map(|index| format!("b{index}")).collect();
    let mut script = echo_script(&names);
    script.push((
        "z\n".to_string(),
        Reply::Status(500, error_body("server_error", "api_error")),
    ));
    let model = FakeModel::start_by_content(script);
    let dir = TempDir::new("t21-quiet");
    dir.write_config(&model_config_with(&model, 4, ""));
    let pager = recording_pager(&dir, "quiet");
    let input = format!("{}\nz\n", page(&names));
    let run = run_pager_tty(
        &dir,
        &["--lang", LANG],
        input.as_bytes(),
        &[("MAN_AI_PAGER", pager.to_str().unwrap())],
    );
    assert_eq!(run.code, 0);
    // 只有进度行：500 诊断在分页器启动之后，必须丢弃。
    assert_eq!(run.stderr, PROGRESS);
    let output = text(&run.output);
    assert!(
        output.contains("── man-ai: partially translated (30/31 blocks, backend error) ──"),
        "{output}"
    );
    assert!(!output.contains("HTTP 500"), "{output}");
}

#[test]
fn epipe_停止请求与写入并透传分页器退出码() {
    // 每块译文 1000 字节、共 300 块：写出总量远超管道缓冲（macOS 约 76 KB），
    // 分页器读一点就退出，后续写入必然 EPIPE。
    let translation = format!("{}\n", "y".repeat(1000));
    let name = "x".to_string();
    let names: Vec<String> = (0..300).map(|_| name.clone()).collect();
    let model = FakeModel::start_by_content(vec![(
        format!("{name}\n"),
        Reply::Ok(completion(&translation, "stop")),
    )]);
    let dir = TempDir::new("t21-epipe");
    dir.write_config(&model_config_with(&model, 4, ""));
    let pager = quitting_pager(&dir, "quit");
    let input = page(&names);
    let run = run_pager_tty(
        &dir,
        &["--lang", LANG],
        input.as_bytes(),
        &[("MAN_AI_PAGER", pager.to_str().unwrap())],
    );
    assert_eq!(run.code, 7, "{}", run.stderr);
    let output = text(&run.output);
    assert!(!output.contains("man-ai: translation"), "{output}");
    assert!(!output.contains("translated"), "{output}");
    // 请求在写端坏掉后停止：远少于 200 块。
    assert!(model.count() < 150, "{}", model.count());
}

#[test]
fn 第一次_ctrl_c_取消翻译并照常启动分页器() {
    let names = vec!["a".to_string(), "b".to_string()];
    let model = FakeModel::start_by_content(vec![
        (
            "a\n".to_string(),
            Reply::Delay(Duration::from_secs(3), completion("A\n", "stop")),
        ),
        (
            "b\n".to_string(),
            Reply::Delay(Duration::from_secs(3), completion("B\n", "stop")),
        ),
    ]);
    let dir = TempDir::new("t21-sigint");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = run_pager_tty_with_pid(
        &dir,
        &["--lang", LANG],
        page(&names).as_bytes(),
        &[],
        move |pid| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while model.count() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(150));
            assert!(send_sigint(pid), "发送 SIGINT");
        },
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        text(&run.output),
        format!(
            "{HEADER}a\n\nb\n\n── man-ai: translation failed (interrupted), remaining blocks left untranslated ──\n"
        )
    );
    assert_eq!(run.stderr, PROGRESS);
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
