//! 预转换与 Markdown 译文协议（§6.3、§7.2–§7.4）。
//!
//! 假端点下非 TTY 的 `--translate`：请求送预转换后的 Markdown，stdout 只有
//! 译文 Markdown 源文，块数与请求数一致，失败块回退为预转换原文。

mod common;

use common::{FakeModel, Reply, TempDir, completion, error_body, model_config, run_pipe, text};

const LANG: &str = "zh-CN";

/// 页眉、SGR 粗体、退格叠加下划线三块，混排两种样式来源。
fn styled_page() -> Vec<u8> {
    b"LS(1)\n\n\x1b[1mls\x1b[0m - list\n\nu\x08_s\x08_e\x08_ - notice\n".to_vec()
}

fn styled_script() -> Vec<(String, Reply)> {
    vec![
        ("LS(1)\n".to_string(), Reply::Ok(completion("LS(1)\n", "stop"))),
        (
            "**ls** - list\n".to_string(),
            Reply::Ok(completion("**ls** - 列出\n", "stop")),
        ),
        (
            "*use* - notice\n".to_string(),
            Reply::Ok(completion("*用途* - 提示\n", "stop")),
        ),
    ]
}

#[test]
fn 非_tty_translate_输出为_markdown_源文且请求送预转换文本() {
    let model = FakeModel::start_by_content(styled_script());
    let dir = TempDir::new("t25-markdown");
    dir.write_config(&model_config(&model, ""));
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], &styled_page(), &[]);
    assert_eq!(run.code, 0);
    // stdout 只有纯 Markdown 源文：无 ANSI 序列、无退格、无页首/尾行。
    assert_eq!(text(&run.stdout), "LS(1)\n\n**ls** - 列出\n\n*用途* - 提示\n");
    assert!(run.stderr.is_empty(), "{}", text(&run.stderr));

    // 块数与请求数一致；每块的用户消息就是预转换文本。
    let requests = model.requests();
    let users: Vec<String> = requests
        .iter()
        .map(|request| {
            request.body["messages"][1]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        users,
        vec!["LS(1)\n", "**ls** - list\n", "*use* - notice\n"]
    );
    // 系统提示是 §7.2 的 Markdown 允许清单版本。
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Your output is Markdown."));
    assert!(system.contains("Never output tables, links, images, HTML, or block quotes."));
}

#[test]
fn 失败块回退为预转换原文而非渲染字节() {
    let model = FakeModel::start_by_content(vec![
        ("plain\n".to_string(), Reply::Ok(completion("甲\n", "stop"))),
        (
            "**bold**\n".to_string(),
            Reply::Status(500, error_body("server_error", "api_error")),
        ),
    ]);
    let dir = TempDir::new("t25-fallback-pre");
    dir.write_config(&model_config(&model, ""));
    let input = b"plain\n\n\x1b[1mbold\x1b[0m\n";
    let run = run_pipe(&dir, &["--translate", "--lang", LANG], input, &[]);
    assert_eq!(run.code, 0);
    // 回退块是预转换原文：粗体标记可见，原始 SGR 字节不出现。
    assert_eq!(text(&run.stdout), "甲\n\n**bold**\n");
    let stderr = text(&run.stderr);
    assert!(stderr.contains("backend returned HTTP 500"), "{stderr}");
    // 部分回退不写整页失败提示。
    assert!(!stderr.contains("translation failed"), "{stderr}");
    // 空响应走 §7.4：拒绝后重试一次。
    assert_eq!(model.count(), 3);
}
