//! #23：`-k`/`-f` 拦截与描述批翻译（§5.4、§7.6、§13.3）。

mod common;

use std::fs;

use common::{
    FakeModel, Reply, TempDir, completion, error_body, fake_kman, model_config, recording_pager,
    run_shell_capture, run_shell_tty, text, write_script,
};

const LANG: &str = "zh-CN";
const HEADER: &str = "── man-ai: translated (Simplified Chinese) ──\n\n";

/// 结果行 fixture：padding、逗号、省略号、无分隔行、非 UTF-8 行、空描述、描述里第二个分隔符。
fn fixture() -> Vec<u8> {
    let mut bytes =
        b"ls, list    - list directory contents\ncd - change the directory\nweird... - does x - y\n"
            .to_vec();
    bytes.extend_from_slice(b"no separator line\nbad - \xff byte\nempty - \n");
    bytes
}

/// 翻译后的同一 fixture（名字列、行尾与不可翻译行逐字节不变）。
fn translated() -> Vec<u8> {
    let mut bytes = "ls, list    - 列出目录内容\ncd - 切换目录\nweird... - 做 x - y\n"
        .as_bytes()
        .to_vec();
    bytes.extend_from_slice(b"no separator line\nbad - \xff byte\nempty - \n");
    bytes
}

fn path_with(dir: &TempDir) -> String {
    format!("{}:/usr/bin:/bin", dir.path().display())
}

fn cache_env(dir: &TempDir) -> Vec<(&'static str, String)> {
    let path = dir.file("cache");
    vec![("XDG_CACHE_HOME", path.to_str().unwrap().to_string())]
}

fn env_refs<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    env.iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

/// `v1` 下的条目文件数（不含 tmp）。
fn entries(dir: &TempDir) -> usize {
    let v1 = dir.file("cache").join("man-ai").join("v1");
    fs::read_dir(&v1)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|shard| shard.file_name() != "tmp")
        .map(|shard| {
            fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .flatten()
                .count()
        })
        .sum()
}

#[test]
fn 非_tty_直写译文且请求体是描述数组() {
    let dir = TempDir::new("t23-pipe");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 3);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(run.code, 3, "非 TTY 退出码取 man");
    assert_eq!(run.output, translated());
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    assert_eq!(model.count(), 1, "整批一个请求");
    assert_eq!(std::fs::read_to_string(record).unwrap(), "cat");

    let request = &model.requests()[0];
    assert!(
        request.body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with(
                "You translate the description column of man -k output into Simplified Chinese."
            ),
        "{}",
        request.body
    );
    assert_eq!(
        request.body["messages"][1]["content"],
        r#"["list directory contents","change the directory","does x - y"]"#
    );
}

#[test]
fn tty_写页首行经分页器且无尾行() {
    let dir = TempDir::new("t23-tty");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "list"],
        &[("PATH", &path), ("MAN_AI_PAGER", "cat")],
    );

    assert_eq!(run.code, 0);
    let mut expected = HEADER.as_bytes().to_vec();
    expected.extend_from_slice(&translated());
    assert_eq!(run.output, expected);
    assert_eq!(
        text(&run.output).matches("── man-ai:").count(),
        1,
        "只有页首行，没有尾行"
    );
    assert_eq!(std::fs::read_to_string(record).unwrap(), "cat");
}

#[test]
fn 未匹配时不发请求不起分页器不写页首行() {
    let dir = TempDir::new("t23-nomatch");
    let model = FakeModel::start(vec![]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, b"", 1);
    let pager = recording_pager(&dir, "nomatch");
    let path = path_with(&dir);
    let pager_path = pager.to_str().unwrap();

    let unix = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "zzz"],
        &[("PATH", &path), ("MAN_AI_PAGER", pager_path)],
    );
    assert_eq!(unix.code, 1, "无匹配透传 man 退出码");
    assert!(unix.output.is_empty(), "{:?}", unix.output);
    assert!(!dir.file("nomatch.start").exists(), "不应起分页器");

    let piped = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "zzz"],
        &[("PATH", &path)],
    );
    assert_eq!(piped.code, 1);
    assert!(piped.output.is_empty());
    assert_eq!(model.count(), 0);
}

#[test]
fn 数组为空时原样交付且不写页首行() {
    let page = b"no separator\nanother line\n";
    let dir = TempDir::new("t23-empty-array");
    let model = FakeModel::start(vec![]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, page, 0);
    let pager = recording_pager(&dir, "empty");
    let path = path_with(&dir);

    let unix = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "x"],
        &[("PATH", &path), ("MAN_AI_PAGER", pager.to_str().unwrap())],
    );
    assert_eq!(unix.code, 0);
    assert_eq!(unix.output, page, "原样交分页器，无页首行");
    assert!(dir.file("empty.start").exists(), "结果仍走分页器");

    let piped = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "x"],
        &[("PATH", &path)],
    );
    assert_eq!(piped.output, page);
    assert!(piped.stderr.is_empty());
    assert_eq!(model.count(), 0);
}

#[test]
fn 整批失败时_tty_页首行用失败形态() {
    let dir = TempDir::new("t23-tty-fail");
    let model = FakeModel::start(vec![Reply::Status(
        401,
        error_body("invalid_api_key", "invalid_request_error"),
    )]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 2);
    let path = path_with(&dir);

    let run = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "list"],
        &[("PATH", &path), ("MAN_AI_PAGER", "cat")],
    );

    assert_eq!(run.code, 0, "TTY 退出码取分页器");
    assert_eq!(model.count(), 1, "401 不重试");
    let mut expected =
        b"\xe2\x94\x80\xe2\x94\x80 man-ai: translation failed (backend error), showing the original below \xe2\x94\x80\xe2\x94\x80\n\n"
            .to_vec();
    expected.extend_from_slice(&fixture());
    assert_eq!(run.output, expected);
    assert!(
        run.stderr
            .contains("man-ai: backend returned HTTP 401; check api_key / api_key_env"),
        "{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("showing the original below"),
        "TTY 不在 stderr 重复失败行: {}",
        run.stderr
    );
}

#[test]
fn 整批失败时非_tty_写_stderr_一行再写原文() {
    let dir = TempDir::new("t23-pipe-fail");
    let model = FakeModel::start(vec![Reply::Status(500, error_body("oops", "server_error"))]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 4);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(run.code, 4);
    assert_eq!(model.count(), 2, "500 重试一次");
    assert_eq!(run.output, fixture());
    assert!(
        run.stderr
            .contains("man-ai: translation failed (backend error), showing the original below"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("man-ai: backend returned HTTP 500"),
        "{}",
        run.stderr
    );
}

#[test]
fn 批校验失败重试一次后全部保留原文() {
    let dir = TempDir::new("t23-invalid");
    let model = FakeModel::start(vec![
        Reply::Ok(completion(r#"["only one"]"#, "stop")),
        Reply::Ok(completion(r#"["a\nb","c"]"#, "stop")),
    ]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(model.count(), 2, "校验失败重试一次");
    assert_eq!(run.output, fixture(), "重试仍失败则全部原文");
}

#[test]
fn 截断响应重试一次后采用() {
    let dir = TempDir::new("t23-length");
    let model = FakeModel::start(vec![
        Reply::Ok(completion(
            r#"["列出目录内容","切换目录","做 x - y"]"#,
            "length",
        )),
        Reply::Ok(completion(
            r#"["列出目录内容","切换目录","做 x - y"]"#,
            "stop",
        )),
    ]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(model.count(), 2);
    assert_eq!(run.output, translated());
}

#[test]
fn 整批一个缓存条目且第二次零请求() {
    let dir = TempDir::new("t23-cache");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);
    let args = ["--translate", "--lang", LANG, "-k", "list"];
    let mut extra = vec![("PATH", path.as_str())];
    extra.extend_from_slice(&env);

    let first = run_shell_capture(&dir, &args, &extra);
    assert_eq!(model.count(), 1);
    assert_eq!(first.output, translated());
    assert_eq!(entries(&dir), 1, "整批一个条目");

    let second = run_shell_capture(&dir, &args, &extra);
    assert_eq!(model.count(), 1, "命中不应再发请求");
    assert_eq!(second.output, first.output);
    assert!(second.stderr.is_empty(), "{}", second.stderr);
}

#[test]
fn 关闭缓存时不建目录() {
    let dir = TempDir::new("t23-cache-off");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, "cache_enabled = false\n"));
    let env = cache_env(&dir);
    let env = env_refs(&env);
    fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);
    let mut extra = vec![("PATH", path.as_str())];
    extra.extend_from_slice(&env);

    let run = run_shell_capture(&dir, &["--translate", "--lang", LANG, "-k", "list"], &extra);
    assert_eq!(run.output, translated());
    assert!(!dir.file("cache").exists(), "关闭缓存不应创建目录");
}

#[test]
fn f_与_k_行为一致() {
    let dir = TempDir::new("t23-f");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--lang", LANG, "-f", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(run.output, translated());
    assert_eq!(model.count(), 1);
    assert_eq!(std::fs::read_to_string(record).unwrap(), "cat");
}

#[test]
fn 带_p_时让路不捕获() {
    let dir = TempDir::new("t23-pager");
    let model = FakeModel::start(vec![]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "-P", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(run.output, fixture());
    assert_eq!(std::fs::read_to_string(record).unwrap(), "<unset>");
    assert_eq!(model.count(), 0);
    assert!(
        run.stderr.contains("-P overrides MANPAGER"),
        "{}",
        run.stderr
    );
}

#[test]
fn 长选项_pager_也_让路() {
    let dir = TempDir::new("t23-long-pager");
    let model = FakeModel::start(vec![]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 0);
    let path = path_with(&dir);

    let run = run_shell_capture(
        &dir,
        &["--translate", "--pager=less", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );

    assert_eq!(run.output, fixture());
    assert_eq!(std::fs::read_to_string(record).unwrap(), "<unset>");
    assert_eq!(model.count(), 0);
}

#[test]
fn 写入口选项与_no_translate_不拦截() {
    for flag in ["-w", "-M", "-S", "-t", "-T"] {
        let dir = TempDir::new("t23-passthrough");
        let model = FakeModel::start(vec![]);
        dir.write_config(&model_config(&model, ""));
        let record = fake_kman(&dir, &fixture(), 0);
        let path = path_with(&dir);

        let run = run_shell_capture(
            &dir,
            &["--translate", flag, "--lang", LANG, "-k", "list"],
            &[("PATH", &path)],
        );
        assert_eq!(run.output, fixture(), "{flag}");
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            "<unset>",
            "{flag}"
        );
        assert_eq!(model.count(), 0, "{flag}");
    }

    let dir = TempDir::new("t23-no-translate");
    let model = FakeModel::start(vec![]);
    dir.write_config(&model_config(&model, ""));
    let record = fake_kman(&dir, &fixture(), 4);
    let path = path_with(&dir);
    let run = run_shell_tty(
        &dir,
        &["--no-translate", "--lang", LANG, "-k", "list"],
        &[("PATH", &path)],
    );
    assert_eq!(run.output, fixture());
    assert_eq!(run.code, 4);
    assert_eq!(std::fs::read_to_string(record).unwrap(), "<unset>");
    assert_eq!(model.count(), 0);
}

#[test]
fn 原_manpager_仍是_k_结果的内层分页器() {
    let dir = TempDir::new("t23-manpager");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 0);
    let pager = recording_pager(&dir, "user");
    let path = path_with(&dir);

    let run = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "list"],
        &[("PATH", &path), ("MANPAGER", pager.to_str().unwrap())],
    );

    assert_eq!(run.code, 0);
    assert!(dir.file("user.start").exists(), "应使用用户原来的 MANPAGER");
    let mut expected = HEADER.as_bytes().to_vec();
    expected.extend_from_slice(&translated());
    assert_eq!(run.output, expected);
}

#[test]
fn tty_退出码取分页器() {
    let dir = TempDir::new("t23-exit-pager");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 2);
    let pager = dir.file("nine-pager");
    write_script(&pager, "#!/bin/sh\ncat >/dev/null\nexit 9\n");
    let path = path_with(&dir);

    let run = run_shell_tty(
        &dir,
        &["--lang", LANG, "-k", "list"],
        &[("PATH", &path), ("MAN_AI_PAGER", pager.to_str().unwrap())],
    );
    assert_eq!(run.code, 9, "退出码取分页器而不是 man");
}

#[test]
fn 分页器链不可用时直写_stdout_并取_man_退出码() {
    let dir = TempDir::new("t23-no-pager");
    let model = FakeModel::start(vec![Reply::Ok(completion(
        r#"["列出目录内容","切换目录","做 x - y"]"#,
        "stop",
    ))]);
    dir.write_config(&model_config(&model, ""));
    fake_kman(&dir, &fixture(), 3);
    // PATH 只含 fixture 目录：less/cat 都解析不到，链不可用。
    let path = dir.path().display().to_string();

    let run = run_shell_tty(&dir, &["--lang", LANG, "-k", "list"], &[("PATH", &path)]);

    assert_eq!(run.code, 3);
    let mut expected = HEADER.as_bytes().to_vec();
    expected.extend_from_slice(&translated());
    assert_eq!(run.output, expected);
}
