//! 薄壳模式（规格 §4.3、§5.1）。
//!
//! 只处理自有参数、`MANPAGER` 与入口判定，页面检索和渲染全部交给平台 `man`。

use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::{cli, pager};

/// `-P` 让路时写到 stderr 的一行（§4.3）。程序输出一律英文。
pub const PAGER_OVERRIDE_NOTE: &str = "man-ai: -P overrides MANPAGER; not translating";

/// 设置 `MANPAGER` 后 `exec man`。
///
/// 只有 exec 失败（`man` 不存在）时才返回，退出码 127。
pub fn run(own: &cli::OwnArgs, lang: Option<&str>) -> i32 {
    let scan = cli::scan_entry(&own.rest);
    if scan.pager_override {
        eprintln!("{PAGER_OVERRIDE_NOTE}");
        return exec_man(&own.rest);
    }
    if scan.passthrough {
        return exec_man(&own.rest);
    }
    // -k/-f 拦截（§5.4）与非 TTY 的 --translate 捕获（§5.3）由后续票接入，
    // 本票两条路径都只透传。
    save_manpager();
    set_env("MANPAGER", manpager_value(lang, own.translate));
    exec_man(&own.rest)
}

/// `MANPAGER="man-ai [--lang <规范标签>] [--translate|--no-translate]"`（§4.2）。
fn manpager_value(lang: Option<&str>, translate: Option<bool>) -> String {
    let mut value = String::from("man-ai");
    if let Some(tag) = lang {
        value.push_str(" --lang ");
        value.push_str(tag);
    }
    match translate {
        Some(true) => value.push_str(" --translate"),
        Some(false) => value.push_str(" --no-translate"),
        None => {}
    }
    value
}

/// 把用户原有的 `MANPAGER` 存进 `MAN_AI_PAGER`；指向自身时不写（§5.1 第 8 条）。
fn save_manpager() {
    let Some(original) = env::var_os("MANPAGER") else {
        return;
    };
    if original.is_empty() {
        return;
    }
    if original.to_str().is_some_and(pager::points_to_self) {
        return;
    }
    set_env("MAN_AI_PAGER", original);
}

fn set_env(key: &str, value: impl Into<OsString>) {
    // SAFETY: 薄壳在 exec man 之前始终单线程，没有并发读取环境。
    unsafe { env::set_var(key, value.into()) };
}

fn exec_man(args: &[OsString]) -> i32 {
    let _ = Command::new("man").args(args).exec();
    127
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manpager_值按规格拼接() {
        assert_eq!(manpager_value(None, None), "man-ai");
        assert_eq!(manpager_value(Some("zh-CN"), None), "man-ai --lang zh-CN");
        assert_eq!(manpager_value(None, Some(true)), "man-ai --translate");
        assert_eq!(
            manpager_value(Some("ja"), Some(false)),
            "man-ai --lang ja --no-translate"
        );
    }
}
