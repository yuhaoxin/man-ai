//! 翻译管线：按块序并发请求、重试状态机、回退（规格 §7.7、§8.1、§8.5）。
//!
//! `buffered(concurrency)` 给出在途上限并保持产出顺序；重试在同一个槽位内完成。
//! 写出顺序恒等块序：已完成的后续块留在内存等前序。

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream;

use crate::backend::{Backend, Completion, Failure, Reason};
use crate::blocks::{Block, SplitPage};
use crate::language::Language;
use crate::output::Out;
use crate::{prompt, validate};

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
    /// 在途请求上限（§8.1）；取值已由配置校验，至少 1。
    pub concurrency: usize,
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
/// 非 UTF-8 块不送模型、原样输出，也不计入 `total`/`adopted`。
pub async fn run_page(page: SplitPage, ctx: PipelineCtx<'_>, out: &mut Out) -> io::Result<Outcome> {
    out.write_header(ctx.lang.display())?;
    let system = prompt::page(ctx.lang);
    let total = page
        .blocks
        .iter()
        .filter(|block| block.text.is_some())
        .count();
    let stop = AtomicBool::new(false);
    let interrupt = out.interrupt_flag().unwrap_or_default();
    let mut adopted = 0;
    let mut failures: Vec<Reason> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    let mut deterministic = false;

    let backend = ctx.backend;
    let system = system.as_str();
    let stop_ref = &stop;
    let interrupt_ref = &interrupt;
    let results = stream::iter(page.blocks)
        .map(move |mut block| async move {
            let Some(text) = block.text.take() else {
                return BlockResult::original(block);
            };
            /* 确定性失败或第一次 Ctrl-C 之后，未开始的块不再发请求。 */
            if stop_ref.load(Ordering::Relaxed) || interrupt_ref.load(Ordering::Relaxed) {
                return BlockResult::original(block);
            }
            let Some(backend) = backend else {
                stop_ref.store(true, Ordering::Relaxed);
                return BlockResult::failed(block, Failure::new(Reason::NotConfigured, false));
            };
            match translate_block(backend, system, &text, stop_ref, interrupt_ref).await {
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
        .buffered(ctx.concurrency.max(1));
    futures::pin_mut!(results);

    while let Some(result) = results.next().await {
        if let Some(failure) = &result.failure {
            if let Some(line) = &failure.diagnostic {
                if !diagnostics.iter().any(|seen| seen == line) {
                    diagnostics.push(line.clone());
                    out.diagnostic(line);
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
        let body = match &result.translation {
            Some(translation) => translation.as_bytes(),
            None => &result.block.raw,
        };
        out.write_block(&result.block, body)?;
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
    Ok(outcome)
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
    /// 输入有样式、输出一条不剩：先重试一次，仍没有就接受。
    StyleLost(String),
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
            Ok(completion) => evaluate(input, &completion),
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
            Verdict::StyleLost(text) if stopped || retried => return Ok(text),
            Verdict::StyleLost(_) => {
                retried = true;
                tokio::time::sleep(RETRY_DELAY).await;
            }
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

/// §7.4 加工 + §7.5 校验。
fn evaluate(input: &str, completion: &Completion) -> Verdict {
    if completion.finish_reason.as_deref() == Some("length") {
        return Verdict::Rejected;
    }
    let text = validate::postprocess(&completion.content);
    if text.trim().is_empty() || validate::structural(&text).is_err() {
        return Verdict::Rejected;
    }
    if validate::has_styles(input) && !validate::has_styles(&text) {
        return Verdict::StyleLost(text);
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
            finish_reason: finish.map(str::to_string),
        }
    }

    #[test]
    fn 校验拒绝空响应与截断() {
        assert!(matches!(
            evaluate("x", &completion("   \n", Some("stop"))),
            Verdict::Rejected
        ));
        assert!(matches!(
            evaluate("x", &completion("ok", Some("length"))),
            Verdict::Rejected
        ));
        assert!(matches!(
            evaluate("x", &completion("\x08bad", Some("stop"))),
            Verdict::Rejected
        ));
    }

    #[test]
    fn 样式丢失先标为重试() {
        let input = "\x1b[1mbold\x1b[0m";
        assert!(matches!(
            evaluate(input, &completion("粗体", Some("stop"))),
            Verdict::StyleLost(_)
        ));
        assert!(matches!(
            evaluate(input, &completion("\x1b[1m粗体\x1b[0m", Some("stop"))),
            Verdict::Accepted(_)
        ));
        assert!(matches!(
            evaluate("plain", &completion("纯文本", Some("stop"))),
            Verdict::Accepted(_)
        ));
    }
}
