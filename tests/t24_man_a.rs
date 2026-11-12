//! `man -a` 的按页集成验收（§3.2、§13.1）。
//!
//! `-a` 完全交给平台 man：每个匹配页各起一次 man-ai，各自判定 TTY、各自翻译与缓存、
//! 各自写页首行。假 man 复现「每页各起一次 MANPAGER」，不用真 man 的 whatis 数据库。

mod common;

use std::fs;

use common::{
    FakeModel, Reply, TempDir, completion, model_config, run_shell_tty, text, write_script,
};

const LANG: &str = "zh-CN";
const HEADER: &str = "── man-ai: translation (Simplified Chinese) ──\n\n";
const TAIL: &str = "── man-ai: translated (Simplified Chinese) ──\n";

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
    let record = fake_multi_man(&dir, &["alpha\n", "beta\n"]);
    let model = FakeModel::start_by_content(vec![
        (
            "alpha\n".to_string(),
            Reply::Ok(completion("t-alpha\n", "stop")),
        ),
        (
            "beta\n".to_string(),
            Reply::Ok(completion("t-beta\n", "stop")),
        ),
    ]);
    dir.write_config(&model_config(&model, ""));
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let cache = dir.file("cache");
    let cache = cache.to_str().unwrap();

    let expected_page = |body: &str| format!("{HEADER}{body}\n{TAIL}");
    let expected = expected_page("t-alpha\n") + &expected_page("t-beta\n");

    let first = run_shell_tty(
        &dir,
        &["--lang", LANG, "-a", "fixture"],
        &[
            ("PATH", &path),
            ("MAN_AI_PAGER", "cat"),
            ("XDG_CACHE_HOME", cache),
        ],
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(text(&first.output), expected);
    assert_eq!(fs::read_to_string(&record).unwrap(), "-a fixture");
    assert_eq!(model.count(), 2, "每页一次请求，各页独立");
    // 每页各写一次进度行。
    assert_eq!(
        first
            .stderr
            .matches("man-ai: translating (Simplified Chinese)…\n")
            .count(),
        2,
        "{}",
        first.stderr
    );

    // 第二次：两页都命中缓存，请求数不增。
    let second = run_shell_tty(
        &dir,
        &["--lang", LANG, "-a", "fixture"],
        &[
            ("PATH", &path),
            ("MAN_AI_PAGER", "cat"),
            ("XDG_CACHE_HOME", cache),
        ],
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(text(&second.output), expected);
    assert_eq!(model.count(), 2, "命中缓存后不再请求");
    // 分页器启动前的进度行与是否命中缓存无关，每页各一次。
    assert_eq!(
        second
            .stderr
            .matches("man-ai: translating (Simplified Chinese)…\n")
            .count(),
        2,
        "{}",
        second.stderr
    );
}
