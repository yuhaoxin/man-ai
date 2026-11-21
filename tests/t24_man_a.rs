//! `man -a` 的按页集成验收（§3.2、§13.1）。
//!
//! `-a` 完全交给平台 man：每个匹配页各起一次 man-ai，各自判定 TTY、各自翻译与
//! 缓存、各自进阅读器。假 man 复现「每页各起一次 MANPAGER」，不用真 man 的
//! whatis 数据库。

mod common;

use std::fs;
use std::time::Duration;

use common::{
    FakeModel, Reply, TempDir, compact, completion, model_config, run_pty_driven, wait_seen,
    write_script,
};

const LANG: &str = "zh-CN";

fn page_dir(dir: &TempDir) -> std::path::PathBuf {
    let bin = dir.file("bin");
    fs::create_dir_all(&bin).unwrap();
    bin
}

/// 假 man：记录收到的 argv，然后对每个「匹配页」各跑一次 `$MANPAGER`。
fn fake_multi_man(dir: &TempDir, pages: &[&str]) -> std::path::PathBuf {
    for (index, page) in pages.iter().enumerate() {
        fs::write(dir.file(&format!("page{index}.bin")), page).unwrap();
    }
    let record = dir.file("man-args.txt");
    let body: String = (0..pages.len())
        .map(|index| {
            format!(
                "eval \"$MANPAGER\" < '{}' || exit $?\n",
                dir.file(&format!("page{index}.bin")).display()
            )
        })
        .collect();
    write_script(
        &dir.file("bin/man"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n{body}exit 0\n",
            record.display()
        ),
    );
    record
}

#[test]
fn man_a_每页各自翻译与缓存且与单页一致() {
    let dir = TempDir::new("t24-a");
    let bin = page_dir(&dir);
    // MANPAGER 里的 man-ai 从 PATH 解析（§4.2）。
    write_script(
        &bin.join("man-ai"),
        &format!("#!/bin/sh\nexec '{}' \"$@\"\n", common::BIN),
    );
    let record = fake_multi_man(&dir, &["ALPHA(1)\n", "BETA(1)\n"]);
    let model = FakeModel::start_by_content(vec![
        (
            "ALPHA(1)\n".to_string(),
            Reply::Ok(completion("t-alpha\n", "stop")),
        ),
        (
            "BETA(1)\n".to_string(),
            Reply::Ok(completion("t-beta\n", "stop")),
        ),
    ]);
    dir.write_config(&model_config(&model, ""));
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let cache = dir.file("cache");
    let env = vec![
        ("PATH", path),
        ("XDG_CACHE_HOME", cache.to_str().unwrap().to_string()),
    ];
    let stderr_file = dir.file("stderr.txt");
    let script = format!(
        "exec '{}' --lang {LANG} -a fixture 2>'{}'",
        common::BIN,
        stderr_file.display()
    );

    // 每页的阅读器各等尾行 + 正文，然后按 q。
    let run = || {
        run_pty_driven(
            &dir,
            &script,
            &stderr_file,
            &env,
            "── man-ai: translation (Simplified Chinese) ──",
            move |_pid, writer, seen| {
                wait_seen(
                    seen,
                    "t-alpha──man-ai:translated(SimplifiedChinese)──",
                    Duration::from_secs(30),
                );
                let _ = writer.write_all(b"q");
                wait_seen(
                    seen,
                    "t-beta──man-ai:translated(SimplifiedChinese)──",
                    Duration::from_secs(30),
                );
                let _ = writer.write_all(b"q");
            },
        )
    };

    let first = run();
    assert_eq!(first.code, 0, "{}", first.stderr);
    let text = compact(&first.text());
    // 两页按序各进一次阅读器。
    assert!(text.contains("t-alpha"), "{text}");
    assert!(text.contains("t-beta"), "{text}");
    assert!(
        text.find("t-alpha").unwrap() < text.find("t-beta").unwrap(),
        "页序应与 man 的输出一致：{text}"
    );
    assert_eq!(fs::read_to_string(&record).unwrap(), "-a fixture");
    assert_eq!(model.count(), 2, "每页一次请求，各页独立");
    assert!(first.stderr.is_empty(), "{}", first.stderr);

    // 第二次：两页都命中缓存，请求数不增。
    let second = run();
    assert_eq!(second.code, 0, "{}", second.stderr);
    let text = compact(&second.text());
    assert!(text.contains("t-alpha"), "{text}");
    assert!(text.contains("t-beta"), "{text}");
    assert_eq!(model.count(), 2, "命中缓存后不再请求");
    assert!(second.stderr.is_empty(), "{}", second.stderr);
}
