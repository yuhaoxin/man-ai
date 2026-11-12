//! 入口：模式判定、`--help`/`--version`、`--lang` 校验、退出码。

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::process::exit;

use man_ai::backend::Backend;
use man_ai::language::Language;
use man_ai::translate::PipelineCtx;
use man_ai::{blocks, cli, config, output, pager, shellmode, translate};

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
        shellmode::run(&own, lang.as_ref().map(Language::tag))
    } else {
        pager_mode(&own, lang)
    };
    exit(code);
}

/// `--lang` 缺值、空值与形状非法都是用法错误，退出 2（§4.4、§11.6）。
fn resolve_lang(own: &cli::OwnArgs) -> Result<Option<Language>, i32> {
    if let Some(message) = &own.lang_error {
        eprintln!("man-ai: {message}");
        return Err(2);
    }
    match own.lang.as_deref() {
        None => Ok(None),
        Some(raw) => match Language::parse_cli(raw) {
            Ok(lang) => Ok(Some(lang)),
            Err(message) => {
                eprintln!("man-ai: {message}");
                Err(2)
            }
        },
    }
}

/// 分页器模式：先解析配置与语言并输出告警，再读完 stdin 决定输出路径（§5.2）。
fn pager_mode(own: &cli::OwnArgs, cli_lang: Option<Language>) -> i32 {
    let loaded = config::load(config::config_path().as_deref());
    let config_lang = loaded
        .config
        .lang
        .as_deref()
        .and_then(Language::from_config);
    let lang = Language::resolve(cli_lang, config_lang);
    // §11.3：主标签为 en 时整条路径不写 stderr，配置告警也不例外。
    if !lang.is_english() {
        for warning in &loaded.warnings {
            eprintln!("{warning}");
        }
    }

    let mut input = Vec::new();
    let _ = io::stdin().read_to_end(&mut input);
    let page = blocks::split(&input);

    // §5.2 第 5 条：en、--no-translate、非 TTY 且没有 --translate、0 块都原文直通。
    let to_translate = io::stdout().is_terminal() || own.translate == Some(true);
    let translate_page = !lang.is_english()
        && own.translate != Some(false)
        && to_translate
        && !page.blocks.is_empty();
    if !translate_page {
        return output_original(own, &input);
    }

    let backend = match Backend::from_config(&loaded.config) {
        Ok(backend) => Some(backend),
        Err(failure) => {
            if let Some(line) = failure.diagnostic {
                eprintln!("man-ai: {line}");
            }
            None
        }
    };

    let tty = io::stdout().is_terminal();
    let mut out = output::Out::new(tty);
    let ctx = PipelineCtx {
        lang: &lang,
        backend: backend.as_ref(),
    };
    let outcome = match translate::block_on(translate::run_page(page, ctx, &mut out)) {
        Ok(outcome) => outcome,
        Err(_) => return 0,
    };
    if !tty && outcome.all_failed() {
        if let Some(reason) = outcome.reason {
            eprintln!(
                "man-ai: translation failed ({}), showing the original below",
                reason.as_str()
            );
        }
    }
    out.finish(&own.rest).unwrap_or(0)
}

/// 原文直通：TTY 走内层分页器链，非 TTY 直写 stdout（§5.2 第 5 条）。
fn output_original(own: &cli::OwnArgs, input: &[u8]) -> i32 {
    if io::stdout().is_terminal() {
        if let Some(chain) = pager::resolve() {
            if let Ok(code) = pager::pipe(&chain, &own.rest, input) {
                return code;
            }
        }
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(input);
    let _ = out.flush();
    0
}
