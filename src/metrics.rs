//! 整页指标（规格 §8.3）：注入时钟与 usage 事件的纯函数聚合，无 IO。
//!
//! 阅读器每帧把整页流逝时间传给 [`PageMetrics::status_line`]；被采用请求的
//! [`Measurement`] 随 [`PageMetrics::record`] 进入。回退、失败尝试与被取消
//! 请求由调用方保证不进这里（§8.3 计数范围），磁盘缓存命中不产生事件。

use std::time::Duration;

use crate::backend::Usage;

/// 字段分隔符（§8.3）。
const SEP: &str = " · ";
/// 无被采用请求时的 TPS 记号（§8.3）。
const NO_ADOPTED_TPS: &str = "–";

/// 一个被采用请求的测量记录（§8.3）：usage 与该次尝试的生成窗口成对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// usage 缺失即 `None`；存在时字段缺失也按未知，不按 0 补。
    pub usage: Option<Usage>,
    pub window: Duration,
}

/// 整页指标：只累计被采用请求的测量记录。
#[derive(Default)]
pub struct PageMetrics {
    /// 按到达序。
    events: Vec<Measurement>,
    /// 整页缓存命中：零请求，只显示 elapsed 与 cache hit（§8.3）。
    pub cache_hit: bool,
}

impl PageMetrics {
    /// 一个被采用请求处置完成。
    pub fn record(&mut self, measurement: Measurement) {
        self.events.push(measurement);
    }

    /// 状态行（§8.3）：字段固定顺序 `elapsed · out · tps · cached · in`；
    /// 超宽从右向左省略字段，至少保留 elapsed 与 out；`note` 是 §8.5 注记，
    /// 在指标字段之外，计入行宽但从不省略。
    pub fn status_line(&self, elapsed: Duration, note: Option<&str>, width: usize) -> String {
        let mut fields = vec![
            format!("elapsed {:.1}s", elapsed.as_secs_f32()),
            format!("out {}", self.out()),
            format!("tps {}", self.tps()),
            format!("cached {}", self.cached()),
            format!("in {}", self.input()),
        ];
        if self.cache_hit {
            fields.truncate(1);
            fields.push("cache hit".to_string());
        }
        while fields.len() > 2 && joined(&fields, note).chars().count() > width {
            fields.pop();
        }
        joined(&fields, note)
    }

    /// out：Σ `completion_tokens`；缺失写 `?`，混合写 `<已知>+?`（§8.3）。
    fn out(&self) -> String {
        let (sum, missing) = self.out_total();
        known_plus(sum, missing)
    }

    /// out 总数与缺 `completion_tokens` 的请求数（§8.3）；out 与 tps 共用。
    /// usage 整块缺失也按缺字段计。
    fn out_total(&self) -> (u64, usize) {
        let mut sum = 0u64;
        let mut missing = 0;
        for event in &self.events {
            match event.usage.and_then(|usage| usage.completion_tokens) {
                Some(tokens) => sum += u64::from(tokens),
                None => missing += 1,
            }
        }
        (sum, missing)
    }

    /// cached：Σ `cached_tokens`；usage 或 cached 缺失都按未知计（§8.3）。
    fn cached(&self) -> String {
        let missing = self
            .events
            .iter()
            .filter(|event| event.usage.is_none_or(|usage| usage.cached_tokens.is_none()))
            .count();
        let sum: u64 = self
            .events
            .iter()
            .filter_map(|event| event.usage.and_then(|usage| usage.cached_tokens))
            .map(u64::from)
            .sum();
        known_plus(sum, missing)
    }

    /// in：Σ 非缓存输入（§8.3）。任一请求的 usage、prompt 或 cached 未知时
    /// 整体写 `?`。
    fn input(&self) -> String {
        let sum: Option<u64> = self
            .events
            .iter()
            .map(|event| event.usage.as_ref().and_then(non_cached_input))
            .sum();
        sum.map_or_else(|| "?".to_string(), |sum| sum.to_string())
    }

    /// tps：out 总数 ÷ Σ生成窗口（§8.3）。无被采用请求写 `–`，out 缺失或
    /// 零窗口写 `?`；数值按十进制整数显示，截断取整。
    fn tps(&self) -> String {
        if self.events.is_empty() {
            return NO_ADOPTED_TPS.to_string();
        }
        let (out, missing) = self.out_total();
        if missing > 0 {
            return "?".to_string();
        }
        let window: Duration = self.events.iter().map(|event| event.window).sum();
        if window.is_zero() {
            return "?".to_string();
        }
        ((out as f64 / window.as_secs_f64()) as u64).to_string()
    }
}

/// 非缓存输入 = prompt − cached − cache_write（§8.3）；prompt 或 cached 未知
/// 即未知。cache_write 缺失按 0：provider 未写入缓存。
fn non_cached_input(usage: &Usage) -> Option<u64> {
    Some(
        u64::from(usage.prompt_tokens?)
            .saturating_sub(u64::from(usage.cached_tokens?))
            .saturating_sub(u64::from(usage.cache_write_tokens.unwrap_or(0))),
    )
}

/// 字段序列加注记拼成一行。
fn joined(fields: &[String], note: Option<&str>) -> String {
    let mut line = fields.join(SEP);
    if let Some(note) = note {
        line.push_str(SEP);
        line.push_str(note);
    }
    line
}

/// 已知合计与未知数 → 整数 / `?` / `<已知>+?`（§8.3 下限语义；已知 0 不写）。
fn known_plus(sum: u64, missing: usize) -> String {
    match (sum, missing) {
        (_, 0) => sum.to_string(),
        (0, _) => "?".to_string(),
        _ => format!("{sum}+?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u32, completion: u32, cached: Option<u32>, cache_write: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cached_tokens: cached,
            cache_write_tokens: cache_write,
        }
    }

    fn event(usage: Option<Usage>, window: Duration) -> Measurement {
        Measurement { usage, window }
    }

    fn line(metrics: &PageMetrics, elapsed: Duration, width: usize) -> String {
        metrics.status_line(elapsed, None, width)
    }

    #[test]
    fn 无被采用请求_三项为0_tps为长划() {
        let metrics = PageMetrics::default();
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 100),
            "elapsed 1.0s · out 0 · tps – · cached 0 · in 0"
        );
    }

    #[test]
    fn 已知usage按块合计() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(120, 7, Some(64), None)),
            Duration::from_secs(1),
        ));
        metrics.record(event(
            Some(usage(80, 3, Some(30), None)),
            Duration::from_millis(1500),
        ));
        // out=10，窗口 2.5s → tps 4；in = (120−64)+(80−30) = 106。
        assert_eq!(
            line(&metrics, Duration::from_secs(3), 100),
            "elapsed 3.0s · out 10 · tps 4 · cached 94 · in 106"
        );
    }

    #[test]
    fn 非缓存输入扣减cache_write_tokens() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(120, 7, Some(64), Some(16))),
            Duration::from_secs(1),
        ));
        // in = 120 − 64 − 16 = 40；缺失的 cache_write 按 0 扣减。
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 100),
            "elapsed 1.0s · out 7 · tps 7 · cached 64 · in 40"
        );
    }

    #[test]
    fn completion_tokens缺失_out与tps未知不按零补() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(Usage {
                prompt_tokens: Some(100),
                completion_tokens: None,
                cached_tokens: Some(10),
                cache_write_tokens: None,
            }),
            Duration::from_secs(1),
        ));
        // §8.3：字段缺失即未知；out 与 tps 写 ?，已知字段照常给出。
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 100),
            "elapsed 1.0s · out ? · tps ? · cached 10 · in 90"
        );
    }

    #[test]
    fn usage缺失_四项都未知() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(None, Duration::from_secs(1)));
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 100),
            "elapsed 1.0s · out ? · tps ? · cached ? · in ?"
        );
    }

    #[test]
    fn 已知与缺失混合_已知加问号() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(100, 5, Some(50), None)),
            Duration::from_secs(1),
        ));
        metrics.record(event(None, Duration::from_secs(1)));
        // out 已知 5、cached 已知 50；tps 与 in 因缺失写 ?。
        assert_eq!(
            line(&metrics, Duration::from_secs(2), 100),
            "elapsed 2.0s · out 5+? · tps ? · cached 50+? · in ?"
        );
    }

    #[test]
    fn cached缺失_in同为问号() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(100, 5, None, None)),
            Duration::from_secs(1),
        ));
        // out 与 tps 只看 completion_tokens，照常给出。
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 100),
            "elapsed 1.0s · out 5 · tps 5 · cached ? · in ?"
        );
    }

    #[test]
    fn tps按整数截断_零窗口按未知() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(10, 99, Some(0), None)),
            Duration::from_millis(2500),
        ));
        // 99 / 2.5 = 39.6 → 39。
        assert_eq!(
            line(&metrics, Duration::from_secs(3), 100),
            "elapsed 3.0s · out 99 · tps 39 · cached 0 · in 10"
        );
        let mut zero = PageMetrics::default();
        zero.record(event(Some(usage(10, 1, Some(0), None)), Duration::ZERO));
        assert!(line(&zero, Duration::from_secs(1), 100).contains("tps ?"));
    }

    #[test]
    fn 整页缓存命中_只显示elapsed与cache_hit() {
        let metrics = PageMetrics {
            cache_hit: true,
            ..Default::default()
        };
        assert_eq!(
            line(&metrics, Duration::from_millis(500), 100),
            "elapsed 0.5s · cache hit"
        );
    }

    #[test]
    fn 超宽从右向左省略_至少保留elapsed与out() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(120, 7, Some(64), None)),
            Duration::from_secs(1),
        ));
        // 整行 48 字符。
        let full = line(&metrics, Duration::from_secs(1), 100);
        assert_eq!(full.chars().count(), 48, "{full}");
        // 45 宽省略 in；39 宽再省略 cached；20 宽仍保留 elapsed 与 out。
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 45),
            "elapsed 1.0s · out 7 · tps 7 · cached 64"
        );
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 39),
            "elapsed 1.0s · out 7 · tps 7"
        );
        assert_eq!(
            line(&metrics, Duration::from_secs(1), 20),
            "elapsed 1.0s · out 7"
        );
    }

    #[test]
    fn 注记在字段之外_计入行宽但不省略() {
        let mut metrics = PageMetrics::default();
        metrics.record(event(
            Some(usage(120, 7, Some(64), None)),
            Duration::from_secs(1),
        ));
        // 含注记 40 宽：省略 cached 与 in 后 tps 仍放得下。
        assert_eq!(
            metrics.status_line(Duration::from_secs(1), Some("retrying"), 40),
            "elapsed 1.0s · out 7 · tps 7 · retrying"
        );
        // 极窄时注记随 elapsed/out 一起超出宽度，由终端裁剪，字段不再减。
        assert_eq!(
            metrics.status_line(Duration::from_secs(1), Some("retrying"), 10),
            "elapsed 1.0s · out 7 · retrying"
        );
    }
}
