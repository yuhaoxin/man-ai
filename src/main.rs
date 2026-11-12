//! 入口：模式判定、`--help`/`--version`、`--lang` 校验、退出码。

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::process::exit;

use man_ai::{cli, language, pager, shellmode};

const HELP: &str = "\
Usage: man-ai [--lang <tag>] [--translate | --no-translate] [man arguments...]

man-ai is a thin shell and pager around man: pages are rendered by the platform
man and translated into the language chosen by --lang.

Own arguments:
  --help          show this help and exit
  --version       show man-ai's version and exit
  --lang <tag>    target language tag (for example zh-CN); the last one wins
  --translate     translate even when stdout is not a terminal
  --no-translate  never translate

Environment:
  MANPAGER        the thin shell writes itself here; this is the entry point
  MAN_AI_PAGER    man-ai saves your original MANPAGER here for the inner pager chain

All other arguments are passed to man unchanged.
";

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let own = cli::parse_own(&args);

    if own.help {
        print!("{HELP}");
        let _ = io::stdout().flush();
        return;
    }
    if own.version {
        println!("man-ai {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let lang = match resolve_lang(&own) {
        Ok(lang) => lang,
        Err(code) => exit(code),
    };

    let code = if io::stdin().is_terminal() {
        shellmode::run(&own, lang.as_deref())
    } else {
        pager_mode(&own)
    };
    exit(code);
}

/// `--lang` 缺值、空值与形状非法都是用法错误，退出 2（§4.4、§11.6）。
fn resolve_lang(own: &cli::OwnArgs) -> Result<Option<String>, i32> {
    if let Some(message) = &own.lang_error {
        eprintln!("man-ai: {message}");
        return Err(2);
    }
    match own.lang.as_deref() {
        None => Ok(None),
        Some(raw) => match language::cli_tag(raw) {
            Ok(tag) => Ok(Some(tag)),
            Err(message) => {
                eprintln!("man-ai: {message}");
                Err(2)
            }
        },
    }
}

/// 分页器模式：读完 stdin 再决定输出路径（§5.2）。
///
/// 本票不翻译，原文按内层分页器链或 stdout 输出。切块、页首行/尾行与
/// 翻译路径由后续票接入。
fn pager_mode(own: &cli::OwnArgs) -> i32 {
    let mut input = Vec::new();
    let _ = io::stdin().read_to_end(&mut input);

    if io::stdout().is_terminal() {
        if let Some(chain) = pager::resolve() {
            if let Ok(code) = pager::pipe(&chain, &own.rest, &input) {
                return code;
            }
        }
    }

    let mut out = io::stdout().lock();
    let _ = out.write_all(&input);
    let _ = out.flush();
    0
}
