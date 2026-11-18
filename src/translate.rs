//! 翻译管线：按块序并发请求、重试状态机、回退（规格 §7、§8）。
//!
//! `buffered(concurrency)` 给出在途上限并保持产出顺序；重试在同一个槽位内完成。
//! 写出顺序恒等块序：已完成的后续块留在内存等前序。回退与未完成块写预转换原文。

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream;

use crate::backend::{Backend, Completion, Failure, Reason};
use crate::blocks::{Block, SplitPage};
use crate::cache::{self, Cache, KeyParts};
use crate::language::Language;
use crate::output::Out;
use crate::prompt;

/// 重试前的固定等待（§7.7）。
pub const RETRY_DELAY: Duration = Duration::from_millis(500);

/// 轮询第一次 Ctrl-C 的间隔（§5.6）；在途请求据此丢弃。
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// 一次翻译运行需要的上下文。
#[derive(Clone, Copy)]
pub struct PipelineCtx<'a> {
    pub lang: &'a Language,
    /// `None` 表示后端未配置：不发请求，全部回退原文。
    pub backend: Option<&'a Backend>,
    /// 整页文件缓存；关闭或不可用时内部 no-op。
    pub cache: &'a Cache,
    /// 在途请求上限（§8.1）；取值已由配置校验，至少 1。
    pub concurrency: usize,
    /// `--retranslate`：跳过整页缓存读取，全部采用后照常覆盖写（§9.3）。
    pub retranslate: bool,
}

/// 整页运行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 采用译文的块数（不含非 UTF-8 块）。
    pub adopted: usize,
    /// 尝试翻译的块数（不含非 UTF-8 块）。
    pub total: usize,
    /// 回退原因：出现最多者，并列取最早块。
    pub reason: Option<Reason>,
    /// 是否发生确定性失败并停止提交后续块。
    pub deterministic: bool,
    /// 第一次 Ctrl-C 中断了翻译。
    pub interrupted: bool,
}

impl Outcome {
    /// 有块尝试过且没有一块采用。
    pub fn all_failed(&self) -> bool {
        self.total > 0 && self.adopted == 0
    }
}

/// 在独立运行时上跑完一页。
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(future)
}

/// 一块的结果：译文或失败；两者都没有时按原文写出。
struct BlockResult {
    block: Block,
    translation: Option<String>,
    failure: Option<Failure>,
}

impl BlockResult {
    fn original(block: Block) -> Self {
        Self {
            block,
            translation: None,
            failure: None,
        }
    }

    fn failed(block: Block, failure: Failure) -> Self {
        Self {
            block,
            translation: None,
            failure: Some(failure),
        }
    }
}

/// 按块序并发翻译整页并流式写出。
///
/// 每块送预转换文本，失败块写预转换原文（§6.3）；整页缓存命中时零请求直出（§9.3）。
pub async fn run_page(page: SplitPage, ctx: PipelineCtx<'_>, out: &mut Out) -> io::Result<Outcome> {
    out.write_header(ctx.lang.display())?;
    let system = prompt::page(ctx.lang);
    let total = page.blocks.len();
    let stop = AtomicBool::new(false);
    let interrupt = out.interrupt_flag().unwrap_or_default();
    let mut adopted = 0;
    let mut failures: Vec<Reason> = Vec::new();
    let mut diagnosed = false;
    let mut deterministic = false;

    let backend = ctx.backend;
    let cache = ctx.cache;
    let lang_tag = ctx.lang.tag();
    let parts = backend.map(|backend| {
        let (endpoint, model, reasoning_effort) = backend.key_parts();
        KeyParts {
            lang: lang_tag,
            endpoint,
            model,
            reasoning_effort,
        }
    });
    // 整页缓存键：页名从预转换文本第一行页眉解析，哈希对象是整页预转换文本。
    let page_key = parts.and_then(|_| cache::PageKey::parse(pre_text(&page)));

    /* 整页缓存命中：剥注释行直出，零请求（§9.3）；--retranslate 跳过读取。 */
    if !ctx.retranslate {
        if let (Some(parts), Some(key)) = (parts, page_key.as_ref()) {
            if let Some(body) = cache.get_page(&parts, key) {
                let whole = Block {
                    gap_before: 0,
                    raw: Vec::new(),
                    pre: String::new(),
                };
                out.write_block(&whole, body.as_bytes())?;
                let outcome = Outcome {
                    adopted: total,
                    total,
                    reason: None,
                    deterministic: false,
                    interrupted: false,
                };
                if !out.broken() {
                    out.write_tail(&outcome)?;
                }
                return Ok(outcome);
            }
        }
    }

    let system = system.as_str();
    let stop_ref = &stop;
    let interrupt_ref = &interrupt;
    let results = stream::iter(page.blocks)
        .map(move |block| async move {
            /* 确定性失败或第一次 Ctrl-C 之后，未开始的块不再发请求。 */
            if stop_ref.load(Ordering::Relaxed) || interrupt_ref.load(Ordering::Relaxed) {
                return BlockResult::original(block);
            }
            let Some(backend) = backend else {
                stop_ref.store(true, Ordering::Relaxed);
                return BlockResult::failed(block, Failure::new(Reason::NotConfigured, false));
            };
            match translate_block(backend, system, &block.pre, stop_ref, interrupt_ref).await {
                Ok(translation) => BlockResult {
                    block,
                    translation: Some(translation),
                    failure: None,
                },
                Err(failure) => {
                    /* 在这里置位，才能赶在 `buffered` 拉起下一个 future 之前生效。 */
                    if stops_submission(&failure) {
                        stop_ref.store(true, Ordering::Relaxed);
                    }
                    BlockResult::failed(block, failure)
                }
            }
        })
        .buffered(ctx.concurrency);
    futures::pin_mut!(results);

    let mut body = String::new();
    while let Some(result) = results.next().await {
        if let Some(failure) = &result.failure {
            /* 每进程最多输出一条失败诊断，取第一个失败（§10.5）。 */
            if let Some(line) = &failure.diagnostic {
                if !diagnosed {
                    out.diagnostic(line);
                    diagnosed = true;
                }
            }
            if failure.reason == Reason::Interrupted {
                stop.store(true, Ordering::Relaxed);
            } else {
                failures.push(failure.reason);
                if stops_submission(failure) {
                    deterministic = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
        if result.translation.is_some() {
            adopted += 1;
        }
        let body_bytes = match &result.translation {
            Some(translation) => translation.as_bytes(),
            None => result.block.pre.as_bytes(),
        };
        out.write_block(&result.block, body_bytes)?;
        if let Some(translation) = &result.translation {
            body.extend(std::iter::repeat_n('\n', result.block.gap_before));
            body.push_str(translation);
        }
        if let Some(line) = ctx.cache.take_warning() {
            out.diagnostic(&line);
        }
        /* 分页器提前退出：丢弃在途请求、停止写入（§8.2）。 */
        if out.broken() {
            break;
        }
    }

    let interrupted = interrupt.load(Ordering::Relaxed);
    let outcome = Outcome {
        adopted,
        total,
        reason: most_common(&failures),
        deterministic: deterministic || interrupted,
        interrupted,
    };
    if !out.broken() {
        out.write_tail(&outcome)?;
    }
    /* 全部块采用才整页落盘；提前退出时已完整的页照常写（§9.3）。 */
    if adopted == total && total > 0 {
        if let (Some(parts), Some(key)) = (parts, page_key.as_ref()) {
            cache.put_page(&parts, key, &body);
        }
    }
    Ok(outcome)
}

/// 整页预转换文本：全部块 `pre` 按 `gap_before`/`tail_gap` 重组（§9.1 哈希对象）。
fn pre_text(page: &SplitPage) -> String {
    let mut text = String::new();
    for block in &page.blocks {
        text.extend(std::iter::repeat_n('\n', block.gap_before));
        text.push_str(&block.pre);
    }
    text.extend(std::iter::repeat_n('\n', page.tail_gap));
    text
}

/// 确定性失败立即停止提交新块（§8.5）；超时与可重试失败只回退该块。
fn stops_submission(failure: &Failure) -> bool {
    match failure.reason {
        Reason::NotConfigured | Reason::Unreachable | Reason::Interrupted => true,
        Reason::Timeout => false,
        Reason::BackendError => !failure.retryable,
    }
}

/// 出现次数最多的原因；并列时取最先出现的块。
fn most_common(reasons: &[Reason]) -> Option<Reason> {
    let mut best: Option<(Reason, usize)> = None;
    for reason in reasons {
        let count = reasons.iter().filter(|other| *other == reason).count();
        if best.is_none_or(|(_, best_count)| count > best_count) {
            best = Some((*reason, count));
        }
    }
    best.map(|(reason, _)| reason)
}

enum Verdict {
    Accepted(String),
    Rejected,
}

/// 一块的完整尝试：最多重试一次，重试前等 500ms（§7.7）。
///
/// 确定性失败之后不再发起重试；第一次 Ctrl-C 直接放弃该块。
async fn translate_block(
    backend: &Backend,
    system: &str,
    input: &str,
    stop: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<String, Failure> {
    let mut retried = false;
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Err(Failure::new(Reason::Interrupted, false));
        }
        let verdict = match post(backend, system, input, interrupt).await {
            Ok(completion) => evaluate(&completion),
            Err(failure) => {
                if failure.retryable && !retried && !stop.load(Ordering::Relaxed) {
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                return Err(failure);
            }
        };
        let stopped = stop.load(Ordering::Relaxed);
        match verdict {
            Verdict::Accepted(text) => return Ok(text),
            Verdict::Rejected if stopped || retried => {
                return Err(Failure::new(Reason::BackendError, true));
            }
            Verdict::Rejected => {
                retried = true;
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// 单次请求；第一次 Ctrl-C 时丢弃在途 future，不等它返回（§5.6）。
async fn post(
    backend: &Backend,
    system: &str,
    input: &str,
    interrupt: &AtomicBool,
) -> Result<Completion, Failure> {
    let request = backend.post(system, input);
    futures::pin_mut!(request);
    loop {
        tokio::select! {
            result = &mut request => return result,
            () = tokio::time::sleep(CANCEL_POLL) => {
                if interrupt.load(Ordering::Relaxed) {
                    return Err(Failure::new(Reason::Interrupted, false));
                }
            }
        }
    }
}

/// §7.3 返回加工：剥掉首尾纯空行；行内与行尾空格保留（可能是排版）。
fn postprocess(raw: &str) -> String {
    strip_blank_edges(raw)
}

/// 剥掉首尾只含空白的行；中间的行原样保留。
fn strip_blank_edges(text: &str) -> String {
    let lines = lines_with_endings(text);
    let Some(first) = lines.iter().position(|line| !is_blank_line(content(line))) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !is_blank_line(content(line)))
        .unwrap_or(first);
    lines[first..=last].concat()
}

/// 按 `\n` 切行，除最后一行外都带换行符。
fn lines_with_endings(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(offset) = rest.find('\n') {
        lines.push(&rest[..=offset]);
        rest = &rest[offset + 1..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

fn content(line: &str) -> &str {
    line.strip_suffix('\n').unwrap_or(line)
}

fn is_blank_line(text: &str) -> bool {
    text.chars().all(|c| matches!(c, ' ' | '\t' | '\r'))
}

/// §7.4 采用判定：2xx + 未被 length 截断 + 非空白；合法 UTF-8 由 JSON 反序列化保证。
fn evaluate(completion: &Completion) -> Verdict {
    if completion.truncated {
        return Verdict::Rejected;
    }
    let text = postprocess(&completion.content);
    if text.trim().is_empty() {
        return Verdict::Rejected;
    }
    Verdict::Accepted(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 原因统计取最多且并列取最早() {
        use Reason::*;
        assert_eq!(most_common(&[]), None);
        assert_eq!(most_common(&[Timeout]), Some(Timeout));
        assert_eq!(
            most_common(&[Timeout, BackendError, BackendError]),
            Some(BackendError)
        );
        assert_eq!(
            most_common(&[Timeout, BackendError, BackendError, Timeout]),
            Some(Timeout)
        );
        assert_eq!(
            most_common(&[BackendError, Timeout, Timeout, BackendError]),
            Some(BackendError)
        );
    }

    #[test]
    fn 停止提交的判定() {
        use Reason::*;
        assert!(stops_submission(&Failure::new(NotConfigured, false)));
        assert!(stops_submission(&Failure::new(Unreachable, true)));
        assert!(stops_submission(&Failure::new(BackendError, false)));
        assert!(!stops_submission(&Failure::new(BackendError, true)));
        assert!(!stops_submission(&Failure::new(Timeout, false)));
        assert!(stops_submission(&Failure::new(Interrupted, false)));
    }

    fn completion(content: &str, finish: Option<&str>) -> Completion {
        Completion {
            content: content.to_string(),
            truncated: finish == Some("length"),
        }
    }

    #[test]
    fn 采用判定拒绝空白与截断() {
        assert!(matches!(
            evaluate(&completion("   \n", Some("stop"))),
            Verdict::Rejected
        ));
        assert!(matches!(
            evaluate(&completion("ok", Some("length"))),
            Verdict::Rejected
        ));
        assert!(matches!(
            evaluate(&completion("纯文本", Some("stop"))),
            Verdict::Accepted(_)
        ));
        // §7.4：不做内容检查，带控制字符的 2xx 响应照常采用。
        assert!(matches!(
            evaluate(&completion("\x08bad", Some("stop"))),
            Verdict::Accepted(text) if text == "\x08bad"
        ));
    }

    #[test]
    fn 返回加工只剥首尾纯空行() {
        assert_eq!(postprocess("\n\n  x  \n\n"), "  x  \n");
        // 行内与行尾空格保留；中间空行保留；整块空白归零。
        assert_eq!(postprocess("a\n\n \nb \n"), "a\n\n \nb \n");
        assert_eq!(postprocess("  \t\r\n"), "");
        // 恰好是整块围栏时按字面保留，不剥围栏（§7.3）。
        assert_eq!(postprocess("```\nls -l\n```\n"), "```\nls -l\n```\n");
    }
}
