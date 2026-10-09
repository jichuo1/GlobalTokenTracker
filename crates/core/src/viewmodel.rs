//! ViewModels — plain, serializable structs the UI shells render verbatim.
//! All SQL/formatting lives here so shells stay dumb (Mac port = same VMs).

use crate::store::{AppSummary, EventRow, QuotaRow, ShareRow, Store, Totals};
use anyhow::Result;

/// Statistics window selected on the overview page. Persisted as `key` in
/// ui.json so the choice survives restarts (`Custom` bounds persist
/// separately as epoch-ms fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    Today,
    #[default]
    Week,
    Month,
    All,
    /// Arbitrary `[start_ms, end_ms)` window picked on the calendar (local-day
    /// aligned) or the hour dial (local-hour aligned inside one day).
    Custom {
        start_ms: i64,
        end_ms: i64,
    },
}

impl Range {
    pub const LIST: [Range; 4] = [Self::Today, Self::Week, Self::Month, Self::All];

    /// Ordered, non-empty custom window (swaps inverted ends).
    pub fn custom(start_ms: i64, end_ms: i64) -> Self {
        let (s, e) = if start_ms <= end_ms {
            (start_ms, end_ms)
        } else {
            (end_ms, start_ms)
        };
        Self::Custom {
            start_ms: s,
            end_ms: e.max(s + 1),
        }
    }

    /// Inclusive civil-day window `[min(first,last), max(first,last)]` in the
    /// system zone — stored as `[start of first day, start of the day after the
    /// last)`, so DST-short or -long days need no 24h arithmetic.
    pub fn custom_days(first: jiff::civil::Date, last: jiff::civil::Date) -> Self {
        Self::custom_days_in(&jiff::tz::TimeZone::system(), first, last)
    }

    fn custom_days_in(
        tz: &jiff::tz::TimeZone,
        first: jiff::civil::Date,
        last: jiff::civil::Date,
    ) -> Self {
        let (a, b) = if first <= last {
            (first, last)
        } else {
            (last, first)
        };
        let end = b.tomorrow().unwrap_or(b);
        Self::custom(local_day_start_in(tz, a), local_day_start_in(tz, end))
    }

    /// Inclusive hour slots `[min(h1,h2), max(h1,h2)]` (0..=23) of civil day
    /// `day` in the system zone — `[start of the first hour, start of the hour
    /// after the last)`; hour 23 ends at the next local midnight. Wall-clock
    /// based, so a DST gap or overlap hour simply comes out shorter/longer.
    pub fn custom_hours(day: jiff::civil::Date, h1: i8, h2: i8) -> Self {
        Self::custom_hours_in(&jiff::tz::TimeZone::system(), day, h1, h2)
    }

    fn custom_hours_in(tz: &jiff::tz::TimeZone, day: jiff::civil::Date, h1: i8, h2: i8) -> Self {
        let (a, b) = (h1.min(h2).clamp(0, 23), h1.max(h2).clamp(0, 23));
        let at = |h: i8| {
            day.at(h, 0, 0, 0)
                .to_zoned(tz.clone())
                .map(|z| z.timestamp().as_millisecond())
                .unwrap_or_default()
        };
        let end = if b == 23 {
            local_day_start_in(tz, day.tomorrow().unwrap_or(day))
        } else {
            at(b + 1)
        };
        Self::custom(at(a), end)
    }

    /// Trend buckets per local hour: `Today`, or a custom window that stays
    /// inside one local day (a whole day included).
    pub fn hourly(self) -> bool {
        self.hourly_in(&jiff::tz::TimeZone::system())
    }

    fn hourly_in(self, tz: &jiff::tz::TimeZone) -> bool {
        match self {
            Self::Today => true,
            Self::Custom { start_ms, end_ms } => {
                local_date_in(tz, start_ms) == local_date_in(tz, end_ms - 1)
            }
            _ => false,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "今日",
            Self::Week => "近 7 天",
            Self::Month => "近 30 天",
            Self::All => "全部",
            Self::Custom { .. } => "自定义",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week => "week",
            Self::Month => "month",
            Self::All => "all",
            Self::Custom { .. } => "custom",
        }
    }

    /// Rebuild from persisted config: `"custom"` needs its bounds.
    pub fn from_config(key: &str, start_ms: Option<i64>, end_ms: Option<i64>) -> Self {
        if key == "custom"
            && let (Some(s), Some(e)) = (start_ms, end_ms)
        {
            return Self::custom(s, e);
        }
        Self::from_key(key)
    }

    pub fn from_key(s: &str) -> Self {
        match s {
            "today" => Self::Today,
            "month" => Self::Month,
            "all" => Self::All,
            _ => Self::Week,
        }
    }

    pub fn from_label(s: &str) -> Self {
        Self::LIST
            .iter()
            .find(|r| r.label() == s)
            .copied()
            .unwrap_or_default()
    }

    /// Window start (epoch ms); `None` = unbounded ("all").
    pub fn start_ms(self) -> Option<i64> {
        match self {
            Self::Today => Some(day_start_ms(0)),
            Self::Week => Some(day_start_ms(6)),
            Self::Month => Some(day_start_ms(29)),
            Self::All => None,
            Self::Custom { start_ms, .. } => Some(start_ms),
        }
    }

    /// Window end (epoch ms, exclusive); `None` = open-ended.
    pub fn end_ms(self) -> Option<i64> {
        match self {
            Self::Custom { end_ms, .. } => Some(end_ms),
            _ => None,
        }
    }
}

/// Local start-of-day `days_ago` days back, epoch ms (0 = today).
pub fn day_start_ms(days_ago: i64) -> i64 {
    let now = jiff::Zoned::now();
    now.start_of_day()
        .and_then(|d| d.checked_sub(jiff::SignedDuration::from_hours(days_ago * 24)))
        .map(|d| d.timestamp().as_millisecond())
        .unwrap_or_default()
}

/// Local start-of-day (epoch ms) of a civil date in the system time zone.
pub fn local_day_start(d: jiff::civil::Date) -> i64 {
    local_day_start_in(&jiff::tz::TimeZone::system(), d)
}

/// The civil date (system time zone) an epoch-ms instant falls on.
pub fn local_date(ms: i64) -> jiff::civil::Date {
    local_date_in(&jiff::tz::TimeZone::system(), ms)
}

fn local_day_start_in(tz: &jiff::tz::TimeZone, d: jiff::civil::Date) -> i64 {
    d.to_zoned(tz.clone())
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or_default()
}

fn local_date_in(tz: &jiff::tz::TimeZone, ms: i64) -> jiff::civil::Date {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz.clone()).date())
        .unwrap_or(jiff::civil::Date::constant(1970, 1, 1))
}

/// Local start-of-day of the epoch-ms instant (day-align a picker value).
pub fn start_of_local_day(ms: i64) -> i64 {
    jiff::Timestamp::from_millisecond(ms)
        .ok()
        .and_then(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .start_of_day()
                .ok()
                .map(|d| d.timestamp().as_millisecond())
        })
        .unwrap_or(ms)
}

/// One trend bar's data: `date` is "YYYY-MM-DD" (or "HH:00" for the Today
/// range); `top` = the day's top-3 models by tokens, for the hover popup.
#[derive(Debug, Clone)]
pub struct TrendBucket {
    pub date: String,
    pub events: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    pub top: Vec<(String, u64)>,
}

/// One day of the activity heatmap — all four metrics, so the tooltip and the
/// metric switch need no second query. `date` is "YYYY-MM-DD" (local day).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeatDay {
    pub date: String,
    /// Headline tokens: input + output + cache_read.
    pub tokens: u64,
    pub cost_usd: f64,
    pub events: u64,
    /// Sum of the events' `duration_ms` (NULL counts 0).
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct OverviewVm {
    /// Always today — the tray tooltip and badges stay day-scoped regardless
    /// of the selected range.
    pub today: Totals,
    /// Aggregates for the selected `range`.
    pub span: Totals,
    pub all: Totals,
    pub range: Range,
    pub by_app: Vec<AppSummary>,
    /// Per-model totals in the same range — the pie's "按模型" dimension.
    pub by_model: Vec<ShareRow>,
    /// Trend buckets for the selected range: per local day, or per local hour
    /// for `Today` and single-day custom windows (`Range::hourly`). Each
    /// bucket carries its tooltip payload (top-3 models).
    pub daily: Vec<TrendBucket>,
    /// All tool names present in the ledger — the app-filter checkbox list
    /// must show tools even when the filter excludes them.
    pub apps: Vec<String>,
    /// Distinct display-model names scoped by the app filter — the model
    /// checklist. Same invariant: never hidden by the model filter itself.
    pub models: Vec<String>,
    pub quotas: Vec<QuotaRow>,
    /// Quota rows folded into one collapsible group per tool — the quota page
    /// renders these, the overview strip keeps using the flat `quotas`.
    pub quota_groups: Vec<QuotaGroupVm>,
    pub unpriced: Vec<(String, u64)>,
    /// Activity heatmap days — a fixed ~1-year window, independent of `range`
    /// but under the same tool/model filters.
    pub heat: Vec<HeatDay>,
    /// Local UTC offset "+HH:MM" for display.
    pub tz_offset: String,
}

/// One collapsible section on the quota page: every window belonging to a
/// single tool, newest-most-relevant first.
#[derive(Debug, Clone)]
pub struct QuotaGroupVm {
    pub app: String,
    /// UI-facing tool name (`workbuddy` → `WorkBuddy`).
    pub display: String,
    /// Highest used_percent in the group — the header badge.
    pub worst_pct: Option<f64>,
    pub rows: Vec<QuotaRow>,
}

/// Human-readable label for a `window_kind` raw id (spec §6.9 windows).
pub fn quota_kind_label(kind: &str) -> &str {
    match kind {
        "5h_block" => "5 小时窗口",
        "weekly" => "每周限额",
        "monthly" => "每月限额",
        "credits" => "剩余点数",
        "billing_period" => "计费周期",
        "auto_pool" => "Auto 用量池",
        "api_pool" => "API 用量池",
        "session_ctx" => "会话上下文",
        other => other,
    }
}

/// App-id → display name for quota group headers (adapter registry names are
/// not reachable from the view model; keep the two in sync).
pub fn app_display(app: &str) -> &str {
    match app {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "workbuddy" => "WorkBuddy",
        "codebuddy_ide" => "CodeBuddy",
        "codebuddy_cli" => "CodeBuddy CLI",
        "qoder" => "Qoder",
        "opencode" => "OpenCode",
        "zcode" => "ZCode",
        "grok" => "Grok",
        "devin" => "Devin",
        "minimax_code" => "MiniMax Code",
        "kimi_code" => "Kimi Code",
        "cline" => "Cline",
        "commandcode" => "Command Code",
        "gemini_antigravity" => "Antigravity",
        "dsh" => "DeepSeek Harness",
        other => other,
    }
}

/// Fold flat `latest_quotas` rows into per-app groups, ordered by app name.
pub fn group_quotas(rows: Vec<QuotaRow>) -> Vec<QuotaGroupVm> {
    let mut map: std::collections::BTreeMap<String, Vec<QuotaRow>> =
        std::collections::BTreeMap::new();
    for r in rows {
        map.entry(r.app.clone()).or_default().push(r);
    }
    map.into_iter()
        .map(|(app, rows)| QuotaGroupVm {
            display: app_display(&app).to_string(),
            worst_pct: rows.iter().filter_map(|r| r.used_percent).reduce(f64::max),
            app,
            rows,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct DetailVm {
    pub rows: Vec<EventRow>,
    pub total_events: u64,
}

pub fn local_utc_offset() -> String {
    let secs = jiff::Zoned::now().offset().seconds();
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

/// `"+08:00"`/`"-05:30"` → signed milliseconds. `None` on malformed input —
/// callers format `local_utc_offset()` output or validate before use.
pub fn utc_offset_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 6
        || !matches!(b[0], b'+' | b'-')
        || b[3] != b':'
        || ![1, 2, 4, 5].iter().all(|&i| b[i].is_ascii_digit())
    {
        return None;
    }
    let ms = (s[1..3].parse::<i64>().ok()? * 60 + s[4..6].parse::<i64>().ok()?) * 60_000;
    Some(if b[0] == b'-' { -ms } else { ms })
}

impl OverviewVm {
    /// Recompute everything that depends on the range / tool / model filters
    /// from the in-memory cube — microseconds, no SQL. The range-independent
    /// extras (quotas, unpriced list, tz) stay as they are. `false` when the
    /// cube cannot answer exactly (see `Cube::overview_parts`); `self` is then
    /// untouched and the caller takes the SQL path.
    pub fn apply_cube(
        &mut self,
        cube: &crate::cube::Cube,
        range: Range,
        apps: Option<&[String]>,
        models: Option<&[String]>,
    ) -> bool {
        let Some(p) = cube.overview_parts(range, apps, models) else {
            return false;
        };
        self.range = range;
        self.today = p.today;
        self.span = p.span;
        self.all = p.all;
        self.by_app = p.by_app;
        self.by_model = p.by_model;
        self.daily = p.daily;
        self.apps = p.apps;
        self.models = p.models;
        self.heat = cube.heat(apps, models);
        true
    }
}

impl Store {
    /// `overview`, but the aggregates come from `cube`; only the cheap
    /// range-independent extras are queried. Falls back to the SQL path when
    /// the cube declines.
    pub fn overview_from_cube(
        &self,
        cube: &crate::cube::Cube,
        range: Range,
        apps: Option<&[String]>,
        models: Option<&[String]>,
    ) -> Result<OverviewVm> {
        let Some(p) = cube.overview_parts(range, apps, models) else {
            return self.overview(range, apps, models);
        };
        let quotas = self.latest_quotas()?;
        Ok(OverviewVm {
            today: p.today,
            span: p.span,
            all: p.all,
            range,
            by_app: p.by_app,
            by_model: p.by_model,
            daily: p.daily,
            apps: p.apps,
            models: p.models,
            quota_groups: group_quotas(quotas.clone()),
            quotas,
            unpriced: self.unpriced_models()?,
            heat: cube.heat(apps, models),
            tz_offset: local_utc_offset(),
        })
    }

    pub fn overview(
        &self,
        range: Range,
        apps: Option<&[String]>,
        models: Option<&[String]>,
    ) -> Result<OverviewVm> {
        let t0 = day_start_ms(0);
        let start = range.start_ms();
        let end = range.end_ms();
        let tz = local_utc_offset();
        let since = crate::cube::heat_since_ms();
        // Per-bucket × model rows fold into trend buckets carrying the
        // tooltip payload (events + top-3 models by tokens).
        let mut buckets: std::collections::BTreeMap<String, TrendBucket> =
            std::collections::BTreeMap::new();
        for r in self.bucket_models(start, end, range.hourly(), &tz, apps, models)? {
            let key = r.bucket.clone();
            let b = buckets.entry(key.clone()).or_insert_with(|| TrendBucket {
                date: key,
                events: 0,
                tokens: 0,
                cost_usd: 0.0,
                top: Vec::new(),
            });
            b.events += r.events;
            b.tokens += r.tokens;
            b.cost_usd += r.cost_usd;
            b.top.push((r.model, r.tokens));
        }
        let daily: Vec<TrendBucket> = buckets
            .into_values()
            .map(|mut b| {
                b.top.sort_by_key(|m| std::cmp::Reverse(m.1));
                b.top.truncate(3);
                b
            })
            .collect();
        let span = self.totals(start, end, apps, models)?;
        let quotas = self.latest_quotas()?;
        Ok(OverviewVm {
            today: self.totals(Some(t0), None, apps, models)?,
            span,
            all: self.totals(None, None, apps, models)?,
            range,
            by_app: self.by_app(start, end, apps, models)?,
            by_model: self.by_model(start, end, apps, models)?,
            daily,
            apps: self.app_names()?,
            models: self.model_names(apps)?,
            quota_groups: group_quotas(quotas.clone()),
            quotas,
            unpriced: self.unpriced_models()?,
            heat: self.heat_days(apps, models, since)?,
            tz_offset: tz,
        })
    }

    pub fn detail(
        &self,
        page: i64,
        page_size: i64,
        apps: Option<&[String]>,
        models: Option<&[String]>,
    ) -> Result<DetailVm> {
        Ok(DetailVm {
            rows: self.events_page(page_size, page * page_size, apps, models)?,
            total_events: self.event_count(apps, models)?,
        })
    }
}

/// Human formatting helpers shared by CLI and UI.
pub mod fmt {
    /// Exact token count with thousands separators — never abbreviated,
    /// users reconcile these numbers against vendor dashboards.
    pub fn tokens_exact(n: u64) -> String {
        let s = n.to_string();
        let mut out = String::with_capacity(s.len() + s.len() / 3);
        for (i, c) in s.bytes().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(char::from(c));
        }
        out
    }

    /// Float with `prec` decimals then strip insignificant zeros/dot:
    /// trim_f(0.5,4)="0.5", trim_f(45.20,2)="45.2", trim_f(0.0,4)="0".
    /// prec=0 skips trimming so "180" never degrades to "18".
    pub fn trim_f(v: f64, prec: usize) -> String {
        let s = format!("{v:.prec$}");
        if !s.contains('.') {
            return s;
        }
        let t = s.trim_end_matches('0').trim_end_matches('.');
        if t.is_empty() { "0".into() } else { t.into() }
    }

    pub fn usd(v: f64) -> String {
        let prec = if v >= 100.0 {
            0
        } else if v >= 1.0 {
            2
        } else {
            4
        };
        format!("${}", trim_f(v, prec))
    }

    /// Compact count for scan-first reading — 万/亿 at >=10_000 with one
    /// trimmed decimal (64,425→"6.4万", 300,000→"30万"). Used where the
    /// number is ambient context (quota rows); exact counts stay in the
    /// tooltip for reconciliation against vendor dashboards.
    pub fn tokens_compact(n: u64) -> String {
        let trim = |v: f64| {
            let s = format!("{v:.1}");
            s.trim_end_matches(".0").to_string()
        };
        if n >= 100_000_000 {
            format!("{}亿", trim(n as f64 / 1e8))
        } else if n >= 10_000 {
            format!("{}万", trim(n as f64 / 1e4))
        } else {
            tokens_exact(n)
        }
    }

    pub fn tokens_total(t: &crate::store::Totals) -> u64 {
        t.input_tokens
            + t.output_tokens
            + t.cache_read_tokens
            + t.cache_write_tokens
            + t.unclassified_tokens
    }

    /// epoch ms → "MM-DD HH:MM" local.
    pub fn ts_short(ms: Option<i64>) -> String {
        let Some(ms) = ms else { return "—".into() };
        let Ok(t) = jiff::Timestamp::from_millisecond(ms) else {
            return "—".into();
        };
        t.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%m-%d %H:%M")
            .to_string()
    }

    /// epoch ms → "YYYY-MM-DD" local (custom-range bounds display).
    pub fn day(ms: Option<i64>) -> String {
        let Some(ms) = ms else { return "—".into() };
        let Ok(t) = jiff::Timestamp::from_millisecond(ms) else {
            return "—".into();
        };
        t.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d")
            .to_string()
    }

    /// epoch ms → "YYYY-MM-DD HH:MM:SS" local.
    pub fn ts_long(ms: Option<i64>) -> String {
        let Some(ms) = ms else { return "—".into() };
        let Ok(t) = jiff::Timestamp::from_millisecond(ms) else {
            return "—".into();
        };
        t.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// epoch ms → human "3h12m" countdown, "过期" when the instant has passed.
    pub fn until(ms: Option<i64>) -> String {
        let Some(ms) = ms else { return "—".into() };
        let diff = ms - crate::store::now_ms();
        if diff < 0 {
            return "已过期".into();
        }
        let m = diff / 60_000;
        if m >= 1440 {
            format!("{}d{}h", m / 1440, (m % 1440) / 60)
        } else if m >= 60 {
            format!("{}h{}m", m / 60, m % 60)
        } else {
            format!("{m}m")
        }
    }

    pub fn duration(ms: Option<i64>) -> String {
        let Some(ms) = ms else { return "—".into() };
        if ms >= 60_000 {
            format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
        } else if ms >= 1000 {
            format!("{}s", trim_f(ms as f64 / 1000.0, 1))
        } else {
            format!("{ms}ms")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fmt;
    use super::*;
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    #[test]
    fn custom_days_are_inclusive_ordered_and_dst_safe() {
        let utc = TimeZone::UTC;
        let Range::Custom { start_ms, end_ms } =
            Range::custom_days_in(&utc, date(2026, 10, 1), date(2026, 10, 5))
        else {
            panic!("custom")
        };
        assert_eq!(end_ms - start_ms, 5 * 86_400_000);
        assert_eq!(local_date_in(&utc, start_ms), date(2026, 10, 1));
        assert_eq!(local_date_in(&utc, end_ms - 1), date(2026, 10, 5));
        assert_eq!(
            Range::custom_days_in(&utc, date(2026, 10, 5), date(2026, 10, 1)),
            Range::custom_days_in(&utc, date(2026, 10, 1), date(2026, 10, 5)),
        );
        let one = Range::custom_days_in(&utc, date(2026, 10, 3), date(2026, 10, 3));
        assert_eq!(one.end_ms().unwrap() - one.start_ms().unwrap(), 86_400_000);
        // US spring-forward: 2026-03-08 has 23 hours.
        let ny = TimeZone::get("America/New_York").unwrap();
        let r = Range::custom_days_in(&ny, date(2026, 3, 8), date(2026, 3, 8));
        assert_eq!(r.end_ms().unwrap() - r.start_ms().unwrap(), 23 * 3_600_000);
        let r = Range::custom_days_in(&ny, date(2026, 3, 7), date(2026, 3, 9));
        assert_eq!(
            r.end_ms().unwrap() - r.start_ms().unwrap(),
            (24 + 23 + 24) * 3_600_000
        );
        assert_eq!(local_date_in(&ny, r.end_ms().unwrap()), date(2026, 3, 10));
    }

    #[test]
    fn custom_hours_are_inclusive_and_end_at_midnight() {
        let utc = TimeZone::UTC;
        let d = date(2026, 10, 7);
        let day0 = local_day_start_in(&utc, d);
        let r = Range::custom_hours_in(&utc, d, 18, 9);
        assert_eq!(r, Range::custom_hours_in(&utc, d, 9, 18));
        assert_eq!(r.start_ms(), Some(day0 + 9 * 3_600_000));
        assert_eq!(r.end_ms(), Some(day0 + 19 * 3_600_000));
        assert!(r.hourly_in(&utc));
        // Last slot runs to the next local midnight; 0..=23 is the whole day.
        let full = Range::custom_hours_in(&utc, d, 0, 23);
        assert_eq!(full, Range::custom_days_in(&utc, d, d));
        assert!(full.hourly_in(&utc));
        let one = Range::custom_hours_in(&utc, d, 5, 5);
        assert_eq!(one.end_ms().unwrap() - one.start_ms().unwrap(), 3_600_000);
        // US fall-back: 2026-11-01 01:00 happens twice → the 1..=1 slot is 2h.
        let ny = TimeZone::get("America/New_York").unwrap();
        let r = Range::custom_hours_in(&ny, date(2026, 11, 1), 1, 1);
        assert_eq!(r.end_ms().unwrap() - r.start_ms().unwrap(), 2 * 3_600_000);
        let r = Range::custom_hours_in(&ny, date(2026, 11, 1), 0, 23);
        assert_eq!(r.end_ms().unwrap() - r.start_ms().unwrap(), 25 * 3_600_000);
        assert!(r.hourly_in(&ny));
    }

    #[test]
    fn hourly_only_for_today_and_single_day_windows() {
        let utc = TimeZone::UTC;
        assert!(Range::Today.hourly_in(&utc));
        assert!(!Range::Week.hourly_in(&utc));
        assert!(!Range::All.hourly_in(&utc));
        let two = Range::custom_days_in(&utc, date(2026, 10, 6), date(2026, 10, 7));
        assert!(!two.hourly_in(&utc));
    }

    #[test]
    fn local_date_round_trips_through_day_start() {
        for name in ["UTC", "America/New_York", "Asia/Shanghai"] {
            let tz = TimeZone::get(name).unwrap();
            for d in [
                date(2026, 1, 1),
                date(2026, 3, 8),
                date(2026, 11, 1),
                date(2026, 12, 31),
            ] {
                assert_eq!(local_date_in(&tz, local_day_start_in(&tz, d)), d);
            }
        }
    }

    #[test]
    fn tokens_exact_groups() {
        assert_eq!(fmt::tokens_exact(0), "0");
        assert_eq!(fmt::tokens_exact(999), "999");
        assert_eq!(fmt::tokens_exact(1_000), "1,000");
        assert_eq!(fmt::tokens_exact(1_730_848_235), "1,730,848,235");
        assert_eq!(fmt::tokens_exact(13_101_054_884), "13,101,054,884");
        assert_eq!(fmt::tokens_compact(9_999), "9,999");
        assert_eq!(fmt::tokens_compact(10_000), "1万");
        assert_eq!(fmt::tokens_compact(64_425), "6.4万");
        assert_eq!(fmt::tokens_compact(300_000), "30万");
        assert_eq!(fmt::tokens_compact(1_730_848_235), "17.3亿");
    }

    #[test]
    fn trim_f_strips_insignificant_zeros() {
        assert_eq!(fmt::trim_f(0.5, 4), "0.5");
        assert_eq!(fmt::trim_f(0.0038, 4), "0.0038");
        assert_eq!(fmt::trim_f(0.0, 4), "0");
        assert_eq!(fmt::trim_f(45.20, 2), "45.2");
        assert_eq!(fmt::trim_f(45.25, 2), "45.25");
        assert_eq!(fmt::trim_f(180.0, 0), "180"); // prec=0 must not eat integer zeros
        assert_eq!(fmt::trim_f(100.0, 1), "100");
        assert_eq!(fmt::trim_f(2.0, 1), "2");
        assert_eq!(fmt::trim_f(0.00004, 4), "0"); // below precision → clean zero
    }

    #[test]
    fn usd_trims_trailing_zeros() {
        assert_eq!(fmt::usd(0.0), "$0");
        assert_eq!(fmt::usd(0.5), "$0.5");
        assert_eq!(fmt::usd(0.01), "$0.01");
        assert_eq!(fmt::usd(0.0038), "$0.0038");
        assert_eq!(fmt::usd(45.2), "$45.2");
        assert_eq!(fmt::usd(45.25), "$45.25");
        assert_eq!(fmt::usd(180.0), "$180");
    }

    #[test]
    fn quota_groups_fold_per_app() {
        use crate::store::QuotaRow;
        let row = |app: &str, kind: &str, pct: Option<f64>| QuotaRow {
            app: app.into(),
            account: None,
            captured_at: 0,
            window_kind: kind.into(),
            used: None,
            limit_value: None,
            used_percent: pct,
            resets_at: None,
        };
        let groups = super::group_quotas(vec![
            row("workbuddy", "session_ctx", Some(21.5)),
            row("codex", "weekly", Some(3.0)),
            row("codex", "5h_block", Some(81.0)),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].app, "codex");
        assert_eq!(groups[0].rows.len(), 2);
        assert_eq!(groups[0].worst_pct, Some(81.0));
        assert_eq!(groups[0].display, "Codex");
        assert_eq!(groups[1].app, "workbuddy");
        assert_eq!(super::quota_kind_label("session_ctx"), "会话上下文");
        assert_eq!(super::quota_kind_label("brand_new_kind"), "brand_new_kind");
    }

    #[test]
    fn range_roundtrip() {
        for r in super::Range::LIST {
            assert_eq!(super::Range::from_key(r.key()), r);
            assert_eq!(super::Range::from_label(r.label()), r);
        }
    }
}
