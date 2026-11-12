//! `man -k` / `-f` 拦截：按第一个 `" - "` 切分结果行，批翻译描述列（规格 §5.4、§7.6）。
//!
//! 名字列、行尾与无分隔行的字节原样保留；只把描述列的 JSON 数组发一次请求，
//! 整批一个缓存条目，失败时全部描述保留原文。

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use crate::backend::{Backend, Failure, Reason};
use crate::cache::{self, Form, KeyParts};
use crate::config::Config;
use crate::language::Language;
use crate::pager;
use crate::prompt;
use crate::translate;
use crate::validate;

/// 拦截入口：捕获 man 结果、翻译描述列，再按 stdout 是否 TTY 交付。
///
/// `tty` 由调用方传入（`isatty(1)`），避免在这里重复取终端状态。
pub fn run(rest: &[OsString], lang: &Language, config: &Config, tty: bool) -> i32 {
    // §9.5：捕获 man 输出之前剪枝。
    let (cache, warning) = cache::open(config);
    if let Some(line) = warning {
        eprintln!("man-ai: {line}");
    }
    cache.prune(config.cache_max_mb);

    let captured = match Command::new("man")
        .args(rest)
        .env("WHATISPAGER", "cat")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
    {
        Ok(output) => output,
        Err(_) => return 127,
    };
    let man_code = pager::exit_code(captured.status);
    // §5.4 第 3 条：无匹配（捕获为空）时不起分页器、不发请求、不写页首行。
    if captured.stdout.is_empty() {
        return man_code;
    }

    let lines: Vec<&[u8]> = captured
        .stdout
        .split_inclusive(|byte| *byte == b'\n')
        .collect();
    let cuts: Vec<Option<Cut<'_>>> = lines.iter().map(|line| cut(line)).collect();
    let descriptions: Vec<&str> = cuts.iter().flatten().map(|cut| cut.desc).collect();

    // §5.4 第 5 条：没有可翻译描述时结果原样交付。
    if descriptions.is_empty() {
        return deliver(tty, None, &captured.stdout, man_code);
    }

    let json = serde_json::to_vec(&descriptions).expect("descriptions are valid UTF-8");
    let backend = match Backend::from_config(config) {
        Ok(backend) => backend,
        Err(failure) => {
            return fallback(tty, lang, &failure, &captured.stdout, man_code);
        }
    };
    let (endpoint, model, reasoning_effort) = backend.key_parts();
    let parts = KeyParts {
        lang: lang.tag(),
        endpoint,
        model,
        reasoning_effort,
    };

    // 命中直接采用，不重新校验（§9.3）。
    let hit = cache
        .get(Form::K, &parts, &json)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| parse_batch(&text, descriptions.len()));

    let values = match hit {
        Some(values) => values,
        None => match translate::block_on(request(&backend, lang, &json, descriptions.len())) {
            Ok((text, values)) => {
                // §9.3：整批校验通过后写一个条目。
                cache.put(Form::K, &parts, &json, text.as_bytes());
                if let Some(line) = cache.take_warning() {
                    eprintln!("man-ai: {line}");
                }
                values
            }
            Err(failure) => return fallback(tty, lang, &failure, &captured.stdout, man_code),
        },
    };

    let body = assemble(&lines, &cuts, &values);
    deliver(tty, Some(header(lang, None)), &body, man_code)
}

/// 一行在第一个 `" - "` 处的切分结果；`name` 含分隔符。
struct Cut<'a> {
    name: &'a [u8],
    desc: &'a str,
    ending: &'a [u8],
}

/// 只对「有分隔符 + 整行是合法 UTF-8 + 描述非空」的行返回切分；其余整体原样保留。
///
/// 名字列出现非法字节时，该行不送模型也不替换（§5.4 第 5 条）。
fn cut(line: &[u8]) -> Option<Cut<'_>> {
    let (body, ending) = match line.strip_suffix(b"\n") {
        Some(body) => (body, &line[line.len() - 1..]),
        None => (line, &line[line.len()..]),
    };
    let text = std::str::from_utf8(body).ok()?;
    let at = body.windows(3).position(|window| window == b" - ")?;
    // 分隔符是 ASCII，切点必在字符边界上。
    let desc = &text[at + 3..];
    if desc.is_empty() {
        return None;
    }
    Some(Cut {
        name: &body[..at + 3],
        desc,
        ending,
    })
}

fn assemble(lines: &[&[u8]], cuts: &[Option<Cut<'_>>], values: &[String]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut next = 0;
    for (line, cut) in lines.iter().zip(cuts) {
        match cut {
            Some(cut) => {
                body.extend_from_slice(cut.name);
                body.extend_from_slice(values[next].as_bytes());
                body.extend_from_slice(cut.ending);
                next += 1;
            }
            None => body.extend_from_slice(line),
        }
    }
    body
}

/// `-k` 页首行两形态（§5.4、§8.3）；`None` 表示成功。
fn header(lang: &Language, failure: Option<&Failure>) -> String {
    match failure {
        None => format!("── man-ai: translated ({}) ──\n\n", lang.display()),
        Some(failure) => format!(
            "── man-ai: translation failed ({}), showing the original below ──\n\n",
            failure.reason.as_str()
        ),
    }
}

/// 整批失败：诊断与页首/一行 stderr 之后写原文。
fn fallback(tty: bool, lang: &Language, failure: &Failure, original: &[u8], man_code: i32) -> i32 {
    if let Some(line) = &failure.diagnostic {
        eprintln!("man-ai: {line}");
    }
    if tty {
        deliver(tty, Some(header(lang, Some(failure))), original, man_code)
    } else {
        eprintln!(
            "man-ai: translation failed ({}), showing the original below",
            failure.reason.as_str()
        );
        deliver(tty, None, original, man_code)
    }
}

/// 一次请求、最多重试一次（§7.7）；返回校验通过的 JSON 文本与数组。
async fn request(
    backend: &Backend,
    lang: &Language,
    json: &[u8],
    expected: usize,
) -> Result<(String, Vec<String>), Failure> {
    let system = prompt::k(lang);
    let user = std::str::from_utf8(json).expect("JSON is UTF-8");
    let mut retried = false;
    loop {
        match backend.post(&system, user).await {
            Ok(completion) => {
                let text = validate::postprocess(&completion.content);
                if !completion.truncated {
                    if let Some(values) = parse_batch(&text, expected) {
                        return Ok((text, values));
                    }
                }
                if retried {
                    return Err(Failure::new(Reason::BackendError, true));
                }
            }
            Err(failure) => {
                if !failure.retryable || retried {
                    return Err(failure);
                }
            }
        }
        retried = true;
        tokio::time::sleep(translate::RETRY_DELAY).await;
    }
}

/// §7.6 校验：JSON 数组、长度相等、元素是字符串且不含 `\n` / `\r`。
fn parse_batch(text: &str, expected: usize) -> Option<Vec<String>> {
    let values: Vec<serde_json::Value> = serde_json::from_str(text).ok()?;
    if values.len() != expected {
        return None;
    }
    values
        .into_iter()
        .map(|value| {
            let text = value.as_str()?;
            (!text.contains('\n') && !text.contains('\r')).then(|| text.to_string())
        })
        .collect()
}

/// TTY 走内层分页器链并透传其退出码；链不可用时直写 stdout、退出码取 man。
fn deliver(tty: bool, tty_header: Option<String>, body: &[u8], man_code: i32) -> i32 {
    if tty {
        if let Some(chain) = pager::resolve() {
            let mut data = Vec::new();
            if let Some(header) = &tty_header {
                data.extend_from_slice(header.as_bytes());
            }
            data.extend_from_slice(body);
            // -k 的剩余 argv 是检索词，追加给分页器会被当成文件/选项（§5.5 只针对分页器模式）。
            if let Ok(code) = pager::pipe(&chain, &[], &data) {
                return code;
            }
        }
        let mut stdout = io::stdout().lock();
        if let Some(header) = &tty_header {
            let _ = stdout.write_all(header.as_bytes());
        }
        let _ = stdout.write_all(body);
        let _ = stdout.flush();
        return man_code;
    }
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(body);
    let _ = stdout.flush();
    man_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 第一个分隔符切分且名字含分隔符() {
        let cut = cut(b"ls, list    - list directory contents\n").unwrap();
        assert_eq!(cut.name, b"ls, list    - ");
        assert_eq!(cut.desc, "list directory contents");
        assert_eq!(cut.ending, b"\n");
    }

    #[test]
    fn 描述里的后续分隔符属于描述() {
        let cut = cut(b"a... - does x - y\n").unwrap();
        assert_eq!(cut.name, b"a... - ");
        assert_eq!(cut.desc, "does x - y");
    }

    #[test]
    fn 无分隔与非_utf8_不切分() {
        assert!(cut(b"no separator here\n").is_none());
        assert!(cut(b"bad - \xff\n").is_none());
        assert!(cut(b"bad\xffname - valid description\n").is_none());
        assert!(cut(b"empty - \n").is_none());
        assert_eq!(cut(b"tail - x").unwrap().ending, b"");
    }

    #[test]
    fn 批校验按长度与换行拒绝() {
        assert_eq!(
            parse_batch("[\"a\", \"b\"]", 2).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_batch("[\"a\"]", 2).is_none());
        assert!(parse_batch("[\"a\", \"b\"]", 3).is_none());
        assert!(parse_batch("[\"a\n b\"]", 1).is_none());
        assert!(parse_batch("[\"a\", 1]", 2).is_none());
        assert!(parse_batch("not json", 1).is_none());
        assert!(parse_batch("{\"a\":1}", 1).is_none());
    }

    #[test]
    fn 页首行两形态() {
        let lang = Language::parse_cli("zh_CN.UTF-8").unwrap();
        assert_eq!(
            header(&lang, None),
            "── man-ai: translated (Simplified Chinese) ──\n\n"
        );
        assert_eq!(
            header(&lang, Some(&Failure::new(Reason::Timeout, false))),
            "── man-ai: translation failed (timeout), showing the original below ──\n\n"
        );
    }
}
