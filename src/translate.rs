//! 翻译管线：逐块请求、重试状态机、回退（规格 §7.7、§8.5）。
//!
//! 本票串行处理；并发与首屏留给后续票，但写出顺序恒等块序的结构不变。

use std::future::Future;
use std::time::Duration;

use crate::backend::{Backend, Completion, Failure, Reason};
use crate::blocks::SplitPage;
use crate::language::Language;
use crate::output::Out;
use crate::{prompt, validate};

/// 重试前的固定等待（§7.7）。
pub const RETRY_DELAY: Duration = Duration::from_millis(500);

/// 一次翻译运行需要的上下文。
#[derive(Clone, Copy)]
pub struct PipelineCtx<'a> {
    pub lang: &'a Language,
    /// `None` 表示后端未配置：不发请求，全部回退原文。
    pub backend: Option<&'a Backend>,
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

/// 串行翻译整页并按块序写出。
///
/// 非 UTF-8 块不送模型、原样输出，也不计入 `total`/`adopted`。
pub async fn run_page(
    page: SplitPage,
    ctx: PipelineCtx<'_>,
    out: &mut Out,
) -> std::io::Result<Outcome> {
    out.write_header(ctx.lang.display())?;
    let system = prompt::page(ctx.lang);

    let mut adopted = 0;
    let mut total = 0;
    let mut failures: Vec<Reason> = Vec::new();
    let mut deterministic = false;
    let mut stopped = false;
    let mut diagnostics: Vec<String> = Vec::new();

    for mut block in page.blocks {
        let Some(text) = block.text.take() else {
            out.write_block(&block, &block.raw)?;
            continue;
        };
        total += 1;
        if stopped {
            out.write_block(&block, &block.raw)?;
            continue;
        }
        let Some(backend) = ctx.backend else {
            failures.push(Reason::NotConfigured);
            deterministic = true;
            stopped = true;
            out.write_block(&block, &block.raw)?;
            continue;
        };
        match translate_block(backend, &system, &text).await {
            Ok(translation) => {
                adopted += 1;
                out.write_block(&block, translation.as_bytes())?;
            }
            Err(failure) => {
                if let Some(line) = &failure.diagnostic {
                    if !diagnostics.iter().any(|seen| seen == line) {
                        diagnostics.push(line.clone());
                        eprintln!("man-ai: {line}");
                    }
                }
                failures.push(failure.reason);
                if stops_submission(&failure) {
                    deterministic = true;
                    stopped = true;
                }
                out.write_block(&block, &block.raw)?;
            }
        }
    }

    let outcome = Outcome {
        adopted,
        total,
        reason: most_common(&failures),
        deterministic,
    };
    out.write_tail(ctx.lang.display(), &outcome)?;
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
async fn translate_block(backend: &Backend, system: &str, input: &str) -> Result<String, Failure> {
    let mut retried = false;
    loop {
        let verdict = match backend.post(system, input).await {
            Ok(completion) => evaluate(input, &completion),
            Err(failure) => {
                if failure.retryable && !retried {
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                return Err(failure);
            }
        };
        match verdict {
            Verdict::Accepted(text) => return Ok(text),
            Verdict::StyleLost(text) if retried => return Ok(text),
            Verdict::StyleLost(_) => {
                retried = true;
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Verdict::Rejected if retried => {
                return Err(Failure::new(Reason::BackendError, true));
            }
            Verdict::Rejected => {
                retried = true;
                tokio::time::sleep(RETRY_DELAY).await;
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
