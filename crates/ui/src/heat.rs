//! Pure logic behind the overview activity heatmap: which metric is shown,
//! GitHub-style quartile levels, streak/total summary and label formatting.
//! The canvas (`widgets::heatmap`) only draws what these return.

use crate::i18n::{self, Lang};
use crate::tf;
use globaltokentracker_core::viewmodel::{HeatDay, fmt};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeatMetric {
    #[default]
    Tokens,
    Cost,
    Calls,
    Duration,
}

pub const METRICS: [HeatMetric; 4] = [
    HeatMetric::Tokens,
    HeatMetric::Cost,
    HeatMetric::Calls,
    HeatMetric::Duration,
];

impl HeatMetric {
    /// Persisted in ui.json; anything unknown is the default (Tokens).
    pub fn from_key(s: &str) -> Self {
        match s {
            "cost" => Self::Cost,
            "calls" => Self::Calls,
            "duration" => Self::Duration,
            _ => Self::Tokens,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Cost => "cost",
            Self::Calls => "calls",
            Self::Duration => "duration",
        }
    }

    /// zh selector label — callers run it through `tr`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Token",
            Self::Cost => "计费",
            Self::Calls => "调用",
            Self::Duration => "时长",
        }
    }

    pub fn value(self, d: &HeatDay) -> f64 {
        match self {
            Self::Tokens => d.tokens as f64,
            Self::Cost => d.cost_usd,
            Self::Calls => d.events as f64,
            Self::Duration => d.duration_ms as f64,
        }
    }
}

/// 25th/50th/75th percentile of the NON-ZERO values (nearest rank), so one
/// huge day cannot wash out the rest. All zeros when nothing is non-zero.
pub fn thresholds(values: impl Iterator<Item = f64>) -> [f64; 3] {
    let mut v: Vec<f64> = values.filter(|x| *x > 0.0).collect();
    if v.is_empty() {
        return [0.0; 3];
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    [at(0.25), at(0.5), at(0.75)]
}

/// Colour level: 0 = empty, 1–4 = quartile buckets of the non-zero values.
pub fn level(value: f64, t: &[f64; 3]) -> u8 {
    if value <= 0.0 {
        0
    } else if value <= t[0] {
        1
    } else if value <= t[1] {
        2
    } else if value <= t[2] {
        3
    } else {
        4
    }
}

#[derive(Debug, PartialEq)]
pub struct Summary {
    pub active_days: usize,
    pub longest_streak: usize,
    pub total: f64,
}

pub fn summarize(days: &[HeatDay], metric: HeatMetric) -> Summary {
    let (mut active, mut best, mut run, mut total) = (0, 0, 0, 0.0);
    for d in days {
        let v = metric.value(d);
        total += v;
        if v > 0.0 {
            active += 1;
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    Summary {
        active_days: active,
        longest_streak: best,
        total,
    }
}

/// The summary's "合计" figure in the metric's own unit.
pub fn fmt_total(metric: HeatMetric, total: f64) -> String {
    match metric {
        HeatMetric::Tokens => i18n::compact(total as u64),
        HeatMetric::Cost => fmt::usd(total),
        HeatMetric::Calls => fmt::tokens_exact(total as u64),
        HeatMetric::Duration => fmt_span(total as u64),
    }
}

/// "3 小时 12 分" / "45 分" / "30 秒" (sub-second time reads as "0 秒").
pub fn fmt_span(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        tf!("{} 小时 {} 分", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        tf!("{} 分", secs / 60)
    } else {
        tf!("{} 秒", secs)
    }
}

/// Month of an ISO `YYYY-MM-DD` date.
pub fn month_of(date: &str) -> Option<u32> {
    date.get(5..7)?.parse().ok()
}

pub fn month_label(m: u32) -> String {
    const EN: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match i18n::lang() {
        Lang::Zh => format!("{m}月"),
        Lang::En => EN.get(m as usize - 1).copied().unwrap_or("").to_string(),
    }
}

/// `(column, month)` for every column that opens a new month (by its Monday);
/// the first label is dropped when it would collide with the next one.
pub fn month_marks(days: &[HeatDay]) -> Vec<(usize, u32)> {
    let mut marks: Vec<(usize, u32)> = Vec::new();
    let mut prev = None;
    for (c, d) in days.iter().step_by(7).enumerate() {
        let m = month_of(&d.date);
        if m != prev
            && let Some(m) = m
        {
            marks.push((c, m));
        }
        prev = m;
    }
    if marks.len() > 1 && marks[1].0 - marks[0].0 < 3 {
        marks.remove(0);
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(date: &str, tokens: u64, events: u64) -> HeatDay {
        HeatDay {
            date: date.into(),
            tokens,
            events,
            ..Default::default()
        }
    }

    #[test]
    fn metric_keys_round_trip_and_default_to_tokens() {
        for m in METRICS {
            assert_eq!(HeatMetric::from_key(m.key()), m);
        }
        assert_eq!(HeatMetric::from_key(""), HeatMetric::Tokens);
        assert_eq!(HeatMetric::from_key("bogus"), HeatMetric::Tokens);
        assert_eq!(HeatMetric::default(), HeatMetric::Tokens);
    }

    #[test]
    fn quartiles_ignore_zeros_and_one_outlier_cannot_flatten_the_rest() {
        assert_eq!(thresholds([0.0, 0.0].into_iter()), [0.0; 3]);
        let t = thresholds((1..=8).map(f64::from).chain([0.0, 0.0]));
        assert_eq!(t, [3.0, 5.0, 6.0]);
        let levels: Vec<u8> = (0..=9).map(|v| level(f64::from(v), &t)).collect();
        assert_eq!(levels, [0, 1, 1, 1, 2, 2, 3, 4, 4, 4]);
        // A 1000× day only lands in the top bucket; the small days still spread.
        let t = thresholds([1.0, 2.0, 3.0, 4.0, 1_000_000.0].into_iter());
        assert_eq!(level(1.0, &t), 1);
        assert_eq!(level(3.0, &t), 2);
        assert_eq!(level(4.0, &t), 3);
        assert_eq!(level(1_000_000.0, &t), 4);
        // A single non-zero value is its own top bucket boundary.
        let t = thresholds([5.0].into_iter());
        assert_eq!(level(5.0, &t), 1);
    }

    #[test]
    fn summary_counts_active_days_and_longest_run() {
        let days: Vec<HeatDay> = [0, 3, 1, 0, 2, 2, 2, 0]
            .iter()
            .enumerate()
            .map(|(i, &t)| day(&format!("2026-01-{:02}", i + 1), t, 0))
            .collect();
        let s = summarize(&days, HeatMetric::Tokens);
        assert_eq!(
            s,
            Summary {
                active_days: 5,
                longest_streak: 3,
                total: 10.0
            }
        );
        // Another metric over the same days: nothing active.
        assert_eq!(summarize(&days, HeatMetric::Calls).active_days, 0);
    }

    #[test]
    fn month_marks_follow_the_monday_of_each_column() {
        // Mondays: Dec 29, Jan 5, 12, 19, 26, Feb 2 …
        let dates = (29..=31)
            .map(|d| format!("2025-12-{d}"))
            .chain((1..=31).map(|d| format!("2026-01-{d:02}")))
            .chain((1..=8).map(|d| format!("2026-02-{d:02}")));
        let days: Vec<HeatDay> = dates.map(|d| day(&d, 0, 0)).collect();
        let marks = month_marks(&days);
        // Dec label is dropped (next label only 1 column later), Jan @1, Feb @5.
        assert_eq!(marks, [(1, 1), (5, 2)]);
    }
}
