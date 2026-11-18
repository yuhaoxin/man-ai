//! 翻译管线：按块序并发请求、重试状态机、直播写出（规格 §7、§8）。
//!
//! worker 在 `buffered(concurrency)` 里发流式请求，把增量与完成事件发给
//! 单一写出循环；写出循环维护按序前缀：直播块（第一个未完成的块）增量上屏，
//! 后续块静默累积、轮到后整块出现。写出顺序恒等块序；失败与未完成块写预转换原文。

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream;

use crate::backend::{Backend, Completion, Failure, Reason, Usage};
use crate::blocks::{Block, SplitPage};
use crate::cache::{self, Cache, KeyParts};
use crate::language::Language;
use crate::output::Out;
use crate::prompt;
use crate::signals::CancelWatch;

/// 重试前的固定等待（§7.5）。
pub const RETRY_DELAY: Duration = Duration::from_millis(500);

/// worker 发给写出循环的事件；`index` 是块序。
enum Event {
    /// 新到的译文增量（§7.3 累积中的片段）。
    Delta { index: usize, chunk: String },
    /// 重试开始：清空该块已显示内容，从空直播重新开始（§7.5）。
    Reset { index: usize },
    /// 块处置完成：采用、回退或未尝试。
    Finished { index: usize, result: BlockResult },
}

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
    /// 被采用请求的 usage 与生成窗口（§8.3）；失败与未尝试为 `None`。
    usage: Option<Usage>,
    window: Option<Duration>,
}

impl BlockResult {
    fn original(block: Block) -> Self {
        Self {
            block,
            translation: None,
            failure: None,
            usage: None,
            window: None,
        }
    }

    fn failed(block: Block, failure: Failure) -> Self {
        Self {
            block,
            translation: None,
            failure: Some(failure),
            usage: None,
            window: None,
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
    let cancel = out.cancel();
    let cancel_after = cancel.clone();

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
    // 块送进 worker 后页面即被消耗，先把空行数留出来给直播帧用。
    let gaps: Vec<usize> = page.blocks.iter().map(|block| block.gap_before).collect();

    /* 整页缓存命中：剥注释行直出，零请求（§9.3）；--retranslate 跳过读取。 */
    if !ctx.retranslate
        && let (Some(parts), Some(key)) = (parts, page_key.as_ref())
        && let Some(body) = cache.get_page(&parts, key) {
            let whole = Block {
                gap_before: 0,
                raw: Vec::new(),
                pre: String::new(),
            };
            out.write_block(&whole, body.as_str())?;
            /* 状态行从接管起算，只显示流逝时间与 cache hit（§8.3）。 */
            out.cache_hit();
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

    let system = system.as_str();
    let stop_ref = &stop;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let workers = stream::iter(page.blocks.into_iter().enumerate())
        .map(move |(index, block)| {
            let events = events_tx.clone();
            let cancel = cancel.clone();
            async move {
                /* 确定性失败或翻译期间的 Ctrl-C 之后，未开始的块不再发请求。 */
                let result = if stop_ref.load(Ordering::Relaxed) || cancel.triggered() {
                    BlockResult::original(block)
                } else if let Some(backend) = backend {
                    match translate_block(
                        backend,
                        system,
                        &block.pre,
                        index,
                        &events,
                        stop_ref,
                        &cancel,
                    )
                    .await
                    {
                        Ok((translation, usage, window)) => BlockResult {
                            block,
                            translation: Some(translation),
                            failure: None,
                            usage,
                            window: Some(window),
                        },
                        Err(failure) => {
                            /* 在这里置位，才能赶在 `buffered` 拉起下一个 future 之前生效。 */
                            if stops_submission(&failure) {
                                stop_ref.store(true, Ordering::Relaxed);
                            }
                            BlockResult::failed(block, failure)
                        }
                    }
                } else {
                    stop_ref.store(true, Ordering::Relaxed);
                    BlockResult::failed(block, Failure::new(Reason::NotConfigured, false))
                };
                let _ = events.send(Event::Finished { index, result });
            }
        })
        .buffered(ctx.concurrency);
    futures::pin_mut!(workers);

    let mut writer = Writer {
        out,
        gaps,
        acc: vec![String::new(); total],
        pending: (0..total).map(|_| None).collect(),
        live: 0,
        shown: false,
        adopted: 0,
        failures: Vec::new(),
        diagnosed: false,
        deterministic: false,
        body: String::new(),
    };
    let mut done_workers = 0;
    loop {
        if done_workers == total {
            /* worker 已全部返回：收走队列里残留的事件后收尾。 */
            match events_rx.try_recv() {
                Ok(event) => writer.handle(event),
                Err(_) => break,
            }
        } else {
            tokio::select! {
                event = events_rx.recv() => {
                    match event {
                        Some(event) => writer.handle(event),
                        None => break,
                    }
                }
                _ = workers.next() => {
                    done_workers += 1;
                }
            }
        }
        if let Some(line) = ctx.cache.take_warning() {
            writer.out.diagnostic(&line);
        }
        /* 阅读器退出（q）：丢弃在途请求、停止写入（§8.5）。 */
        if writer.out.broken() {
            break;
        }
    }
    let Writer {
        out,
        adopted,
        failures,
        deterministic,
        body,
        ..
    } = writer;

    let interrupted = cancel_after.triggered();
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
    if adopted == total && total > 0
        && let (Some(parts), Some(key)) = (parts, page_key.as_ref()) {
            cache.put_page(&parts, key, &body);
        }
    Ok(outcome)
}

/// 写出循环的按序状态（§8.1）：直播块增量上屏，完成块按序落定。
struct Writer<'a> {
    out: &'a mut Out,
    /// 各块已累积的译文增量；只有直播块的会送到屏幕。
    acc: Vec<String>,
    /// 已完成、还没轮到写出的块。
    pending: Vec<Option<BlockResult>>,
    /// 各块前置空行数；直播块的空行随首个增量帧写进阅读器。
    gaps: Vec<usize>,
    /// 按序前缀中第一个未完成的块（§8.1 直播块）。
    live: usize,
    /// 直播块当前是否有内容在屏幕上；决定回退注记与空行补写。
    shown: bool,
    adopted: usize,
    failures: Vec<Reason>,
    diagnosed: bool,
    deterministic: bool,
    /// 整页译文（全部采用才落盘，§9.3）。
    body: String,
}

impl Writer<'_> {
    fn handle(&mut self, event: Event) {
        match event {
            Event::Delta { index, chunk } => {
                if let Some(text) = self.acc.get_mut(index) {
                    text.push_str(&chunk);
                }
                if index == self.live {
                    let _ = self
                        .out
                        .write_live(self.gaps[self.live], &self.acc[self.live]);
                    self.shown = true;
                }
            }
            Event::Reset { index } => {
                if let Some(text) = self.acc.get_mut(index) {
                    text.clear();
                }
                if index == self.live {
                    /* 重试清空已显示内容，屏幕上只出现当前尝试的内容（§7.5）。 */
                    self.out.note("retrying");
                    let _ = self.out.write_live(self.gaps[self.live], "");
                    self.shown = true;
                }
            }
            Event::Finished { index, result } => {
                if result.translation.is_some() {
                    /* out 随 usage 到达跳增、TPS 同时重算（§8.3）：事件先于落定。 */
                    self.out
                        .metric(result.usage, result.window.unwrap_or_default());
                }
                self.account(&result);
                self.pending[index] = Some(result);
                /* 按序落定：直播块完成即推进，静默累积的块轮到后整块出现。 */
                while self.pending.get(self.live).is_some_and(|slot| slot.is_some()) {
                    let result = self.pending[self.live].take().unwrap();
                    self.finalize(result);
                    self.live += 1;
                    self.shown = false;
                }
            }
        }
    }

    /// 失败诊断与原因统计：每个进程最多一条诊断（§10.5）。
    fn account(&mut self, result: &BlockResult) {
        if let Some(failure) = &result.failure {
            if !self.diagnosed
                && let Some(line) = &failure.diagnostic
            {
                self.out.diagnostic(line);
                self.diagnosed = true;
            }
            if failure.reason != Reason::Interrupted {
                self.failures.push(failure.reason);
                if stops_submission(failure) {
                    self.deterministic = true;
                }
            }
        } else if result.translation.is_some() {
            self.adopted += 1;
        }
    }

    /// 落定一块：采用译文或回退预转换原文（§6.4）；整页译文同步累积。
    fn finalize(&mut self, result: BlockResult) {
        let fallback = match (&result.translation, &result.failure) {
            (Some(_), _) => None,
            /* 直播块被整块替换：状态行挂 fallback 注记（§8.5）。 */
            (None, Some(failure)) if self.shown => Some(failure.reason.as_str()),
            (None, _) => None,
        };
        let text = result
            .translation
            .as_deref()
            .unwrap_or(result.block.pre.as_str());
        let _ = self.out.write_block(&result.block, text);
        /* 注记在 Block 之后发送：Block 会清掉上一条 retrying。 */
        if let Some(reason) = fallback {
            self.out.note(&format!("fallback: {reason}"));
        }
        if let Some(translation) = &result.translation {
            self.body
                .extend(std::iter::repeat_n('\n', result.block.gap_before));
            self.body.push_str(translation);
        }
    }
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

/// 一块的完整尝试：最多重试一次，重试前等 500ms 并清空已显示内容（§7.5）。
///
/// 确定性失败之后不再发起重试；翻译期间的 Ctrl-C 直接放弃该块。
/// 返回译文与被采用请求的 usage 及其生成窗口。
async fn translate_block(
    backend: &Backend,
    system: &str,
    input: &str,
    index: usize,
    events: &tokio::sync::mpsc::UnboundedSender<Event>,
    stop: &AtomicBool,
    cancel: &CancelWatch,
) -> Result<(String, Option<Usage>, Duration), Failure> {
    let mut retried = false;
    loop {
        if cancel.triggered() {
            return Err(Failure::new(Reason::Interrupted, false));
        }
        /* 生成窗口从请求发出起算；只有被采用的那次尝试才上报（§8.3）。 */
        let sent = Instant::now();
        let outcome = match post(backend, system, input, index, events, cancel.clone()).await {
            Ok(completion) => match evaluate(&completion) {
                Verdict::Accepted(text) => Some(Ok((text, completion.usage))),
                Verdict::Rejected => None,
            },
            Err(failure) => Some(Err(failure)),
        };
        let window = sent.elapsed();
        if let Some(result) = outcome {
            return match result {
                Ok((text, usage)) => Ok((text, usage, window)),
                Err(failure) => {
                    if failure.retryable && !retried && !stop.load(Ordering::Relaxed) {
                        retried = true;
                        let _ = events.send(Event::Reset { index });
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    Err(failure)
                }
            };
        }
        /* length 截断或缺 finish_reason：可重试失败，重试后仍失败回退该块（§7.4）。 */
        if retried || stop.load(Ordering::Relaxed) {
            return Err(Failure::new(Reason::BackendError, true));
        }
        retried = true;
        let _ = events.send(Event::Reset { index });
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// 单次请求：增量经 `events` 转发；取消触发时丢弃在途 future，不等它返回（§5.4）。
async fn post(
    backend: &Backend,
    system: &str,
    input: &str,
    index: usize,
    events: &tokio::sync::mpsc::UnboundedSender<Event>,
    mut cancel: CancelWatch,
) -> Result<Completion, Failure> {
    let mut on_delta = |chunk: &str| {
        let _ = events.send(Event::Delta {
            index,
            chunk: chunk.to_string(),
        });
    };
    let request = backend.post(system, input, &mut on_delta);
    futures::pin_mut!(request);
    tokio::select! {
        result = &mut request => result,
        () = cancel.cancelled() => Err(Failure::new(Reason::Interrupted, false)),
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

/// §7.4 采用判定：`finish_reason == "stop"` 且非空白；length 与缺 finish_reason
/// 按 `Verdict::Rejected` 走可重试失败；合法 UTF-8 由 JSON 反序列化保证。
fn evaluate(completion: &Completion) -> Verdict {
    if !completion.stopped {
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
            stopped: finish == Some("stop"),
            usage: None,
        }
    }

    #[test]
    fn 采用判定拒绝空白截断与缺_finish_reason() {
        assert!(matches!(
            evaluate(&completion("   \n", Some("stop"))),
            Verdict::Rejected
        ));
        assert!(matches!(
            evaluate(&completion("ok", Some("length"))),
            Verdict::Rejected
        ));
        // 流结束未见 finish_reason：可重试失败（§7.4）。
        assert!(matches!(evaluate(&completion("ok", None)), Verdict::Rejected));
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
