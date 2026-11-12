//! 自有参数提取/剥离与入口扫描（规格 §3.3、§4.1）。

use std::ffi::OsString;

const LANG_MISSING: &str = "--lang requires a language tag; see man-ai --help";
const LANG_EMPTY: &str = "--lang value is empty; see man-ai --help";

/// 自有参数解析结果。
///
/// `rest` 是不属于 man-ai 的全部参数：`--` 及其后原样保留，顺序不改，
/// 直接交给 `man` 或追加到内层分页器命令行末尾。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OwnArgs {
    pub help: bool,
    pub version: bool,
    /// 最后一次 `--lang` 的原始值（归一化交给 `language`）。
    pub lang: Option<String>,
    /// `Some(true)` = `--translate`，`Some(false)` = `--no-translate`，`None` = 没给。
    pub translate: Option<bool>,
    /// `--lang` 缺值或空值时的报错文案（形状校验由 `language` 负责）。
    pub lang_error: Option<String>,
    pub rest: Vec<OsString>,
}

pub fn parse_own(args: &[OsString]) -> OwnArgs {
    let mut own = OwnArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            own.rest.extend_from_slice(&args[i..]);
            break;
        }
        match arg.to_str() {
            Some("--help") => own.help = true,
            Some("--version") => own.version = true,
            Some("--translate") => own.translate = Some(true),
            Some("--no-translate") => own.translate = Some(false),
            Some("--lang") => match args.get(i + 1) {
                // §11.6：后面紧跟另一个 `--` 开头的参数也算缺值。
                Some(next) if !next.to_string_lossy().starts_with("--") => {
                    set_lang(&mut own, &next.to_string_lossy());
                    i += 1;
                }
                _ => own.lang_error = Some(LANG_MISSING.to_string()),
            },
            Some(s) => match s.strip_prefix("--lang=") {
                Some(value) => set_lang(&mut own, value),
                None => own.rest.push(arg.clone()),
            },
            None => own.rest.push(arg.clone()),
        }
        i += 1;
    }
    own
}

fn set_lang(own: &mut OwnArgs, raw: &str) {
    if raw.trim().is_empty() {
        own.lang = None;
        own.lang_error = Some(LANG_EMPTY.to_string());
    } else {
        own.lang = Some(raw.to_string());
        own.lang_error = None;
    }
}

/// 入口扫描结果。只看 `--` 之前的 argv。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EntryScan {
    /// `-P` / `--pager`（含紧凑写法）。
    pub pager_override: bool,
    /// `-w` / `-M` / `-S` / `-t` / `-T` 任一。
    pub passthrough: bool,
    /// `-k` / `-f`。
    pub kf: bool,
}

const PASSTHROUGH: &[u8] = b"wMStT";
const KF_CLUSTER: &[u8] = b"adfhko";

pub fn scan_entry(rest: &[OsString]) -> EntryScan {
    let mut scan = EntryScan::default();
    for arg in rest {
        let Some(s) = arg.to_str() else { continue };
        if s == "--" {
            break;
        }
        if let Some(long) = s.strip_prefix("--") {
            if long == "pager" || long.starts_with("pager=") {
                scan.pager_override = true;
            }
            continue;
        }
        let Some(cluster) = s.strip_prefix('-') else {
            continue;
        };
        // 从宽：单破折号 token 里出现对应字母即命中。
        if cluster.contains('P') {
            scan.pager_override = true;
        }
        if cluster.bytes().any(|b| PASSTHROUGH.contains(&b)) {
            scan.passthrough = true;
        }
        if cluster.bytes().all(|b| KF_CLUSTER.contains(&b))
            && cluster.bytes().any(|b| b == b'k' || b == b'f')
        {
            scan.kf = true;
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn 剥离自有参数并保留其余参数顺序() {
        let own = parse_own(&args(&[
            "--lang",
            "zh-CN",
            "ls",
            "--translate",
            "-k",
            "foo",
        ]));
        assert_eq!(own.lang.as_deref(), Some("zh-CN"));
        assert_eq!(own.translate, Some(true));
        assert_eq!(own.rest, args(&["ls", "-k", "foo"]));
        assert!(own.lang_error.is_none());
    }

    #[test]
    fn 双破折号之后不再剥离() {
        let own = parse_own(&args(&["--", "--lang", "zh-CN", "--help"]));
        assert_eq!(own.rest, args(&["--", "--lang", "zh-CN", "--help"]));
        assert!(!own.help);
        assert!(own.lang.is_none());
    }

    #[test]
    fn 双破折号之前的帮助与版本优先() {
        let own = parse_own(&args(&["ls", "--help", "--"]));
        assert!(own.help);
        assert_eq!(own.rest, args(&["ls", "--"]));

        let own = parse_own(&args(&["--version"]));
        assert!(own.version);
        assert!(own.rest.is_empty());
    }

    #[test]
    fn 翻译开关最右生效() {
        assert_eq!(
            parse_own(&args(&["--translate", "--no-translate"])).translate,
            Some(false)
        );
        assert_eq!(
            parse_own(&args(&["--no-translate", "--translate"])).translate,
            Some(true)
        );
        assert_eq!(parse_own(&args(&["ls"])).translate, None);
    }

    #[test]
    fn 语言最后生效且支持等号写法() {
        let own = parse_own(&args(&["--lang=ja", "--lang", "ko", "--lang=zh-TW"]));
        assert_eq!(own.lang.as_deref(), Some("zh-TW"));

        let own = parse_own(&args(&["--lang", "ja", "--lang=ko"]));
        assert_eq!(own.lang.as_deref(), Some("ko"));
    }

    #[test]
    fn 语言缺值或空值是用法错误() {
        let own = parse_own(&args(&["--lang"]));
        assert!(own.lang_error.is_some());
        assert_eq!(own.rest, args(&[]));

        let own = parse_own(&args(&["--lang", "--translate"]));
        assert!(own.lang_error.is_some());
        assert_eq!(own.translate, Some(true));

        let own = parse_own(&args(&["--lang="]));
        assert!(own.lang_error.is_some());

        let own = parse_own(&args(&["--lang", "   "]));
        assert!(own.lang_error.is_some());

        // 后面还有合法的 --lang 时以最后一个为准。
        let own = parse_own(&args(&["--lang", "--lang", "zh-CN"]));
        assert_eq!(own.lang.as_deref(), Some("zh-CN"));
        assert!(own.lang_error.is_none());
    }

    #[test]
    fn 未知参数与短选项原样保留() {
        let own = parse_own(&args(&["-h", "-V", "--bogus", "-w"]));
        assert_eq!(own.rest, args(&["-h", "-V", "--bogus", "-w"]));
        assert!(!own.help && !own.version);
    }

    #[test]
    fn 入口扫描_p_从宽() {
        for argv in [
            vec!["-P", "less"],
            vec!["-Pless", "ls"],
            vec!["-aP", "less"],
            vec!["--pager", "less"],
            vec!["--pager=less"],
        ] {
            assert!(
                scan_entry(&args(&argv)).pager_override,
                "{argv:?} 应命中 -P"
            );
        }
        assert!(!scan_entry(&args(&["-p", "x"])).pager_override);
        assert!(!scan_entry(&args(&["--", "-P"])).pager_override);
    }

    #[test]
    fn 入口扫描透传字母() {
        for argv in [
            vec!["-w", "ls"],
            vec!["-M", "/x"],
            vec!["-S", "1"],
            vec!["-t"],
            vec!["-T", "ps"],
        ] {
            assert!(scan_entry(&args(&argv)).passthrough, "{argv:?} 应命中透传");
        }
        assert!(!scan_entry(&args(&["ls", "-a"])).passthrough);
        assert!(!scan_entry(&args(&["--", "-w"])).passthrough);
    }

    #[test]
    fn 入口扫描_kf_聚类() {
        for argv in [
            vec!["-k"],
            vec!["-f"],
            vec!["-ak"],
            vec!["-afk"],
            vec!["-dk"],
        ] {
            assert!(scan_entry(&args(&argv)).kf, "{argv:?} 应命中 -k/-f");
        }
        for argv in [
            vec!["-Qk"],
            vec!["-k5"],
            vec!["-a"],
            vec!["--", "-k"],
            vec!["-a", "--", "-k"],
        ] {
            assert!(!scan_entry(&args(&argv)).kf, "{argv:?} 不应命中 -k/-f");
        }
    }

    #[test]
    fn 入口扫描只看双破折号之前() {
        let scan = scan_entry(&args(&["ls", "--", "-P", "-w", "-k"]));
        assert!(!scan.pager_override && !scan.passthrough && !scan.kf);
    }
}
