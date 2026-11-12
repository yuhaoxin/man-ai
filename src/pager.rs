//! 内层分页器链（规格 §5.5）。

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// 链路最后的候选。
pub const DEFAULT: &str = "less -sR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// 按 shell 词法拆词：空白分词，单双引号与反斜杠转义生效。
///
/// 不支持管道、命令替换与变量展开，`|` 是普通字符。引号不闭合返回 `None`。
pub fn split_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut open = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if open {
                    words.push(std::mem::take(&mut word));
                    open = false;
                }
            }
            '\'' => {
                open = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                open = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                open = true;
                word.push(chars.next()?);
            }
            c => {
                open = true;
                word.push(c);
            }
        }
    }
    if open {
        words.push(word);
    }
    Some(words)
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 首个词按路径或 `PATH` 解析为可执行文件。
fn resolve_executable(word: &str) -> Option<PathBuf> {
    if word.contains('/') {
        let path = PathBuf::from(word);
        return is_executable(&path).then_some(path);
    }
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(word))
        .find(|candidate| is_executable(candidate))
}

/// 命令的首个词解析后是否就是 man-ai 自身（realpath 比较）。
pub fn points_to_self(value: &str) -> bool {
    let Some(words) = split_words(value) else {
        return false;
    };
    words
        .first()
        .and_then(|word| resolve_executable(word))
        .is_some_and(|program| is_self(&program))
}

fn is_self(program: &Path) -> bool {
    let Ok(self_exe) = env::current_exe().and_then(fs::canonicalize) else {
        return false;
    };
    fs::canonicalize(program).is_ok_and(|resolved| resolved == self_exe)
}

/// 按 `MAN_AI_PAGER` → `PAGER` → `less -sR` 解析；不可用的候选跳到下一个。
pub fn resolve() -> Option<Chain> {
    let mut candidates = Vec::new();
    for var in ["MAN_AI_PAGER", "PAGER"] {
        if let Some(value) = env::var_os(var).and_then(|v| v.to_str().map(str::to_owned)) {
            if !value.trim().is_empty() {
                candidates.push(value);
            }
        }
    }
    candidates.push(DEFAULT.to_string());

    for candidate in candidates {
        let Some(words) = split_words(&candidate) else {
            eprintln!("man-ai: cannot split inner pager command, skipping: {candidate}");
            continue;
        };
        if words.is_empty() {
            continue;
        }
        let Some(program) = resolve_executable(&words[0]) else {
            continue;
        };
        if is_self(&program) {
            continue;
        }
        return Some(Chain {
            program,
            args: words[1..].to_vec(),
        });
    }
    None
}

/// 启动内层分页器并写入 `data`，返回其退出码。
///
/// 分页器提前退出导致的写失败按 EPIPE 处理：停止写入，仍等子进程并透传其退出码。
pub fn pipe(chain: &Chain, extra: &[OsString], data: &[u8]) -> io::Result<i32> {
    let mut child = spawn(chain, extra)?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data);
    }
    child.wait().map(exit_code)
}

fn spawn(chain: &Chain, extra: &[OsString]) -> io::Result<Child> {
    Command::new(&chain.program)
        .args(&chain.args)
        .args(extra)
        .stdin(Stdio::piped())
        .spawn()
}

/// 子进程被信号杀死时为 `128 + 信号编号`（§4.4）。
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 空白分词() {
        assert_eq!(split_words("less -sR").unwrap(), ["less", "-sR"]);
        assert_eq!(split_words("  cat \t\n").unwrap(), ["cat"]);
        assert_eq!(split_words("").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn 引号与反斜杠转义() {
        assert_eq!(split_words("pager 'a b'").unwrap(), ["pager", "a b"]);
        assert_eq!(split_words("pager \"a b\"").unwrap(), ["pager", "a b"]);
        assert_eq!(split_words("pager a\\ b").unwrap(), ["pager", "a b"]);
        assert_eq!(split_words("\"a\\\"b\"").unwrap(), ["a\"b"]);
        assert_eq!(split_words("'/bin/p' 'x'").unwrap(), ["/bin/p", "x"]);
        assert_eq!(split_words("cat|wc").unwrap(), ["cat|wc"]);
        assert_eq!(split_words("$HOME/x").unwrap(), ["$HOME/x"]);
    }

    #[test]
    fn 引号不闭合失败() {
        assert_eq!(split_words("pager 'abc"), None);
        assert_eq!(split_words("pager \"abc"), None);
        assert_eq!(split_words("pager abc\\"), None);
    }

    #[test]
    fn 退出码含信号() {
        use std::os::unix::process::ExitStatusExt;
        let exited = ExitStatus::from_raw(3 << 8);
        assert_eq!(exit_code(exited), 3);
        let killed = ExitStatus::from_raw(15);
        assert_eq!(exit_code(killed), 128 + 15);
    }
}
