//! 入口：模式判定、`--help`/`--version`、`--lang` 校验、退出码。

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::process::exit;
use std::sync::mpsc;
use std::thread;

use man_ai::backend::Backend;
use man_ai::cache::{self, Cache};
use man_ai::language::Language;
use man_ai::translate::{self, PipelineCtx};
use man_ai::{blocks, cli, config, output, reader, shellmode, signals};

const HELP: &str = "\
Usage: man-ai [--lang <tag>] [--translate | --no-translate] [man arguments...]

man-ai wraps man through MANPAGER: pages are rendered by the platform man,
translated into the language chosen by --lang, and shown in a built-in
Markdown reader.

Own arguments:
  --help          show this help and exit
  --version       show man-ai's version and exit
  --lang <tag>    target language tag (for example zh-CN); the last one wins
  --translate     translate even when stdout is not a terminal
  --no-translate  never translate

Environment:
  MANPAGER        the thin shell writes itself here; this is the entry point

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
    // §4.2：剥离自有参数后残留的 argv 警告一行后继续。
    if !own.rest.is_empty() {
        let args = own
            .rest
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("man-ai: ignoring pager arguments: {args}");
    }

    // §5.2 第 3 条：确定要走翻译路径时，读 stdin 之前剪枝。
    let will_translate = !lang.is_english()
        && own.translate != Some(false)
        && (io::stdout().is_terminal() || own.translate == Some(true));
    let (cache, cache_warning) = if will_translate {
        cache::open(&loaded.config)
    } else {
        (Cache::disabled(), None)
    };
    if let Some(line) = cache_warning {
        eprintln!("man-ai: {line}");
    }
    if will_translate {
        cache.prune(loaded.config.cache_max_mb);
    }

    let mut input = Vec::new();
    let _ = io::stdin().read_to_end(&mut input);
    let page = blocks::split(&input);

    // §5.2 第 6 条：en、--no-translate、非 TTY 且没有 --translate、0 块都原文直通。
    let to_translate = io::stdout().is_terminal() || own.translate == Some(true);
    let translate_page = !lang.is_english()
        && own.translate != Some(false)
        && to_translate
        && !page.blocks.is_empty();
    if !translate_page {
        return output_original(&input);
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
    if io::stdout().is_terminal() {
        translate_to_reader(
            page,
            &input,
            lang,
            backend,
            cache,
            loaded.config.concurrency,
        )
    } else {
        let ctx = PipelineCtx {
            lang: &lang,
            backend: backend.as_ref(),
            cache: &cache,
            concurrency: loaded.config.concurrency,
        };
        translate_to_stdout(page, ctx)
    }
}

/// 翻译路径的 TTY 端：阅读器接管终端，翻译线程把块按序发进来（§8.1）。
///
/// 翻译线程持有全部所有权的上下文；进程随阅读器退出，在途请求一并丢弃。
fn translate_to_reader(
    page: blocks::SplitPage,
    input: &[u8],
    lang: Language,
    backend: Option<Backend>,
    cache: Cache,
    concurrency: usize,
) -> i32 {
    // 阅读器先接管终端；初始化失败时跳过翻译，预转换原文直写 stdout（§5.4）。
    let mut guard = match reader::takeover() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("man-ai: terminal unavailable ({error}); showing the original");
            write_original(input);
            return 0;
        }
    };
    let (sender, receiver) = mpsc::channel();
    let (cancel_trigger, cancel) = signals::cancel_pair();
    let mut out = output::Out::channel(sender, cancel);
    thread::spawn(move || {
        let ctx = PipelineCtx {
            lang: &lang,
            backend: backend.as_ref(),
            cache: &cache,
            concurrency,
        };
        if translate::block_on(translate::run_page(page, ctx, &mut out)).is_ok() {
            let _ = out.finish();
        }
    });
    let exit = reader::serve_translating(&mut guard, receiver, cancel_trigger);
    for line in &exit.diagnostics {
        eprintln!("man-ai: {line}");
    }
    exit.code
}

/// 翻译路径的非 TTY 端：译文 Markdown 按序直写 stdout（§5.3）。
fn translate_to_stdout(page: blocks::SplitPage, ctx: PipelineCtx<'_>) -> i32 {
    let mut out = output::Out::stdout();
    let outcome = match translate::block_on(translate::run_page(page, ctx, &mut out)) {
        Ok(outcome) => outcome,
        Err(_) => return 0,
    };
    if outcome.all_failed()
        && !out.broken()
        && let Some(reason) = outcome.reason
    {
        eprintln!(
            "man-ai: translation failed ({}), showing the original below",
            reason.as_str()
        );
    }
    out.finish().unwrap_or(0)
}

/// 原文直通（§5.2 第 6 条）：TTY 进阅读器显示预转换原文，非 TTY 直写 stdout。
fn output_original(input: &[u8]) -> i32 {
    if !io::stdout().is_terminal() {
        write_original(input);
        return 0;
    }
    let original = blocks::preconvert(input);
    match reader::takeover() {
        Ok(mut guard) => {
            let exit = reader::serve_static(&mut guard, original);
            for line in &exit.diagnostics {
                eprintln!("man-ai: {line}");
            }
            exit.code
        }
        Err(error) => {
            eprintln!("man-ai: terminal unavailable ({error}); showing the original");
            write_original(input);
            0
        }
    }
}

fn write_original(input: &[u8]) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(input);
    let _ = stdout.flush();
}
