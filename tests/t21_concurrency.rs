//! 按块序并发、重试槽位、超时与确定性失败停止提交（§8.1、§8.5、§8.6）。

mod common;

use std::time::Duration;

use common::{
    FakeModel, Reply, TempDir, completion, error_body, model_config_with, run_pipe, text,
};

const LANG: &str = "zh-CN";

/// `names`（不含换行）按空行分隔构成整页。
fn page(names: &[&str]) -> String {
    format!("{}\n", names.join("\n\n"))
}

fn block_text(name: &str) -> String {
    format!("{name}\n")
}

/// 每块回 `t{i}`，内容与到达顺序无关。
fn echo_script(names: &[&str], delay: Option<Duration>) -> Vec<(String, Reply)> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let reply = completion(&format!("t{index}\n"), "stop");
            let reply = match delay {
                Some(delay) => Reply::Delay(delay, reply),
                None => Reply::Ok(reply),
            };
            (block_text(name), reply)
        })
        .collect()
}

fn translate_pipe(dir: &TempDir, input: &str) -> common::Run {
    run_pipe(dir, &["--translate", "--lang", LANG], input.as_bytes(), &[])
}

#[test]
fn 并发上限为配置值且输出恒按块序() {
    let names = ["b0", "b1", "b2", "b3", "b4", "b5"];
    let model = FakeModel::start_by_content(echo_script(&names, Some(Duration::from_millis(150))));
    let dir = TempDir::new("t21-peak");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = translate_pipe(&dir, &page(&names));
    assert_eq!(run.code, 0, "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "t0\n\nt1\n\nt2\n\nt3\n\nt4\n\nt5\n");
    assert_eq!(model.count(), 6);
    assert_eq!(model.peak_in_flight(), 4);
}

#[test]
fn 并发一退化为串行() {
    let names = ["b0", "b1", "b2", "b3"];
    let model = FakeModel::start_by_content(echo_script(&names, Some(Duration::from_millis(100))));
    let dir = TempDir::new("t21-serial");
    dir.write_config(&model_config_with(&model, 1, ""));
    let run = translate_pipe(&dir, &page(&names));
    assert_eq!(text(&run.stdout), "t0\n\nt1\n\nt2\n\nt3\n");
    assert_eq!(model.count(), 4);
    assert_eq!(model.peak_in_flight(), 1);
}

#[test]
fn 重试在同一个并发槽位内完成() {
    let model = FakeModel::start(vec![
        Reply::Status(500, error_body("server_error", "api_error")),
        Reply::Ok(completion("t0\n", "stop")),
        Reply::Ok(completion("t1\n", "stop")),
    ]);
    let dir = TempDir::new("t21-retry-slot");
    dir.write_config(&model_config_with(&model, 1, ""));
    let run = translate_pipe(&dir, &page(&["b0", "b1"]));
    assert_eq!(run.code, 0, "{}", text(&run.stderr));
    assert_eq!(text(&run.stdout), "t0\n\nt1\n");
    assert_eq!(model.count(), 3);
    assert_eq!(model.peak_in_flight(), 1);
}

#[test]
fn 确定性失败立即停止提交新块() {
    let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let mut script = vec![(
        block_text("a"),
        Reply::Delay(Duration::from_millis(600), completion("A\n", "stop")),
    )];
    script.push((
        block_text("b"),
        Reply::Status(401, error_body("invalid_api_key", "invalid_request_error")),
    ));
    script.extend(echo_script(&names[2..], None));
    let model = FakeModel::start_by_content(script);
    let dir = TempDir::new("t21-stop");
    dir.write_config(&model_config_with(&model, 4, ""));
    let run = translate_pipe(&dir, &page(&names));
    assert_eq!(run.code, 0, "{}", text(&run.stderr));
    // 401 之前已提交的在途块（最多 4 个）可以返回；之后不再有新请求。
    assert!((2..=4).contains(&model.count()), "{}", text(&run.stderr));
    let sent: Vec<String> = model
        .requests()
        .iter()
        .map(|request| {
            request.body["messages"][1]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    for later in ["e\n", "f\n", "g\n", "h\n"] {
        assert!(!sent.iter().any(|content| content == later), "{sent:?}");
    }
    // 未提交的块按序写原文。
    assert!(
        text(&run.stdout).ends_with("e\n\nf\n\ng\n\nh\n"),
        "{}",
        text(&run.stdout)
    );
}

#[test]
fn 超时只回退该块且后续块继续() {
    let mut script = echo_script(&["b1"], None);
    script.push((
        block_text("b0"),
        Reply::Delay(Duration::from_secs(1), completion("超时译文\n", "stop")),
    ));
    let model = FakeModel::start_by_content(script);
    let dir = TempDir::new("t21-timeout");
    dir.write_config(&model_config_with(&model, 4, "timeout_seconds = 0.2\n"));
    let run = translate_pipe(&dir, &page(&["b0", "b1"]));
    assert_eq!(run.code, 0);
    assert_eq!(text(&run.stdout), "b0\n\nt0\n");
    assert_eq!(model.count(), 2);
}
