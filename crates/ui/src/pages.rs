//! Page views — dumb renderers over core ViewModels. All colors/metrics come
//! from `Theme`; overview blocks are config-ordered `widgets` so users can
//! reorder/hide them (persisted in ui.json) without touching code.

use crate::config::{REFRESH_OPTIONS, UiConfig, refresh_label};
use crate::gpu_slide::Slide;
use crate::heat::{HeatMetric, METRICS as HEAT_METRICS};
use crate::i18n::{self, tr};
use crate::theme::Theme;
use crate::updater::UpdateState;
use crate::widgets as w;
use crate::{DETAIL_PAGE_SIZE, MenuKind, Msg, PriceTable, Shell, Snapshot};
use crate::{t, tf};
use globaltokentracker_core::pricing::consensus::Stance;
use globaltokentracker_core::store::{EventRow, PriceRow};
use globaltokentracker_core::update::Release;
use globaltokentracker_core::viewmodel::Range;
use globaltokentracker_core::viewmodel::{app_display, fmt};
use windows_reactor::*;

mod speed;
pub use speed::{SpeedArgs, SpeedMetric, speed_page};

pub fn keyed(views: Vec<View>) -> impl Iterator<Item = KeyedView> {
    views
        .into_iter()
        .enumerate()
        .map(|(i, v)| KeyedView::new(i as u64, v))
}

fn cell(col: i32, v: View) -> View {
    Border::new().grid_column(col).content(v)
}

/// Grid cell with an explicit row — reflow grids need both coordinates.
fn cell_rc(col: i32, row: i32, v: View) -> View {
    Border::new().grid_column(col).grid_row(row).content(v)
}

/// Minimum cell width for the reflow grids (stat cards + share donuts).
const REFLOW_MIN_CELL: f64 = 250.0;

/// Reflow grids never go past 4 across …
const REFLOW_MAX_COLS: usize = 4;

/// … but the width ruler keeps counting `REFLOW_MIN_CELL` steps up to here so
/// the custom-range card can tell very wide windows apart (≥ ~1560 DIP).
pub const WIDTH_TIERS: usize = 6;

/// How many `min_cell`-wide columns fit in `width` DIPs of content
/// (clamped 1..=max; unknown width → `max`, matching the default window).
fn fit_cols(width: f64, min_cell: f64, gap: f64, max: usize) -> usize {
    if width <= 0.0 {
        return max;
    }
    (((width + gap) / (min_cell + gap)).floor() as usize).clamp(1, max)
}

/// N items into `cols` STAR columns × Auto rows — cells stretch to fill
/// the measured width, so wide windows get wide cards, narrow reflow.
fn reflow_grid(theme: &Theme, items: Vec<View>, cols: usize) -> View {
    let cols = cols.clamp(1, items.len().clamp(1, REFLOW_MAX_COLS));
    let rows = items.len().div_ceil(cols);
    Grid::new()
        .columns(vec![GridLength::STAR; cols])
        .rows(vec![GridLength::Auto; rows])
        .column_spacing(theme.gap)
        .row_spacing(theme.gap)
        .keyed_children(items.into_iter().enumerate().map(|(i, v)| {
            KeyedView::new(i as u64, cell_rc((i % cols) as i32, (i / cols) as i32, v))
        }))
}

/// Rows the overview quota card lists before pointing at the Quota page.
const QUOTA_STRIP_ROWS: usize = 6;

/// "已过期" for a window whose reset time has passed, "重置 3h12m" otherwise,
/// nothing when the tool reports no reset time.
fn reset_text(resets_at: Option<i64>) -> String {
    let until = fmt::until(resets_at);
    match resets_at {
        None => String::new(),
        Some(_) if until == "已过期" => tr("已过期").to_string(),
        Some(_) => tf!("重置 {}", tr(&until)),
    }
}

/// Right-aligned, subtle cell text.
fn side_text(theme: &Theme, text: String) -> View {
    TextBlock::new()
        .text(text)
        .font_size(theme.body_size)
        .foreground(theme.subtle)
        .horizontal_alignment(HorizontalAlignment::Right)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// One row of the overview quota card: "Tool · window" (+ account pill), the
/// percent (a tone pill from 50% — sparingly) or credit balance, and when the
/// window resets. Value and reset sit in fixed columns so they line up down
/// the card.
fn quota_strip_row(theme: &Theme, q: &globaltokentracker_core::store::QuotaRow) -> View {
    let mut name: Vec<View> = vec![
        TextBlock::new()
            .text(tf!(
                "{} · {}",
                app_display(&q.app),
                tr(globaltokentracker_core::viewmodel::quota_kind_label(
                    &q.window_kind
                ))
            ))
            .font_size(theme.body_size)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ];
    if let Some(acc) = &q.account {
        name.push(w::badge(theme, acc.clone(), w::BadgeTone::Muted));
    }
    let value: View = match (q.window_kind.as_str(), q.used, q.used_percent) {
        ("credits", Some(u), _) => side_text(theme, tf!("余额 {}", i18n::compact(u as u64))),
        (_, _, Some(p)) if p >= 50.0 => Border::new()
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Center)
            .content(w::badge(
                theme,
                format!("{p:.0}%"),
                if p >= 80.0 {
                    w::BadgeTone::Danger
                } else {
                    w::BadgeTone::Warn
                },
            )),
        (_, _, Some(p)) => side_text(theme, format!("{p:.0}%")),
        _ => side_text(theme, "—".into()),
    };
    bar_row(
        theme,
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .keyed_children(keyed(name)),
        260.0,
        if q.window_kind == "credits" {
            None
        } else {
            q.used_percent
        },
        vec![
            (64.0, value),
            (108.0, side_text(theme, reset_text(q.resets_at))),
        ],
    )
}

/// List row with a share bar between the name and the figures: the bar makes
/// the ranking readable at a glance and fills the gap that used to separate a
/// short name from its numbers on wide windows.
fn usage_row(theme: &Theme, left: String, share_pct: f64, right: String) -> View {
    bar_row(
        theme,
        TextBlock::new()
            .text(left)
            .font_size(theme.body_size)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        200.0,
        Some(share_pct),
        vec![(210.0, side_text(theme, right))],
    )
}

/// Name | bar | fixed-width figure columns. Every row of a list passes the same
/// widths, so the bars start and end on the same x down the whole card (Auto
/// columns would size per row and leave the bars ragged).
fn bar_row(
    theme: &Theme,
    name: View,
    name_w: f64,
    bar_pct: Option<f64>,
    figures: Vec<(f64, View)>,
) -> View {
    // Widths are for the default 12pt body; the figures grow with the font.
    let k = font_scale(theme);
    let mut cols = vec![GridLength::Pixel(name_w * k), GridLength::STAR];
    cols.extend(figures.iter().map(|(w, _)| GridLength::Pixel(*w * k)));
    let mut cells: Vec<View> = vec![cell(0, name)];
    if let Some(p) = bar_pct {
        cells.push(cell(
            1,
            ProgressBar::new()
                .value(p)
                .maximum(100.0)
                .minimum(0.0)
                .margin(Thickness::xy(16.0, 0.0))
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        ));
    }
    for (i, (_, v)) in figures.into_iter().enumerate() {
        cells.push(cell(2 + i as i32, v));
    }
    w::ruled_row(
        theme,
        Grid::new().columns(cols).keyed_children(keyed(cells)),
    )
}

fn vstack(spacing: f64, children: Vec<View>) -> View {
    StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(spacing)
        .keyed_children(keyed(children))
}

fn loading(theme: &Theme, scanning: bool) -> View {
    let mut children: Vec<View> = Vec::new();
    if scanning {
        children.push(
            ProgressRing::new()
                .is_indeterminate(true)
                .is_active(true)
                .into(),
        );
    }
    children.push(
        TextBlock::new()
            .text(t!("正在扫描数据源…"))
            .foreground(theme.subtle)
            .into(),
    );
    StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(12.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .keyed_children(keyed(children))
}

/// Top band of a page: title left, actions right.
fn header(theme: &Theme, title: &str, actions: Vec<View>) -> View {
    // Both sides center on the row: the title is a single line of 22pt text
    // and the action strip is 36 DIP tall, so top-aligned cells left the title
    // riding ~7 DIP above the buttons' optical center on every page.
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .children([
            Border::new()
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    TextBlock::new()
                        .text(title)
                        .font_size(theme.title_size)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .foreground(theme.text)
                        // Long titles ("价目表（$/1M tokens）") wrap instead of
                        // squeezing the action strip to zero on narrow windows.
                        .text_wrapping(TextWrapping::Wrap),
                ),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(10.0)
                        .keyed_children(keyed(actions)),
                ),
        ])
}

/// Custom-range calendar state owned by `Shell`: the month on show and the
/// first click still waiting for its partner.
#[derive(Clone, Copy)]
pub struct CalView {
    pub month: (i16, i8),
    pub anchor: Option<jiff::civil::Date>,
    /// Hour dial's pending first click.
    pub clock_anchor: Option<i8>,
}

fn rgba(c: windows_canvas::ColorF, alpha: f32) -> Color {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::argb(b(c.a * alpha), b(c.r), b(c.g), b(c.b))
}

/// One day cell — a plain `Border` (a real `Button`'s padding clips two-digit
/// days in a narrow cell). Range days get a soft accent wash, the endpoints
/// (or a pending first click) the solid accent, today a hairline outline;
/// days after today are dimmed and inert.
fn cal_day(
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    day: jiff::civil::Date,
    sel: (jiff::civil::Date, jiff::civil::Date),
    today: jiff::civil::Date,
    (cw, ch): (f64, f64),
) -> View {
    let (y, m, d) = (day.year(), day.month(), day.day());
    let accent = theme.accent_cf;
    let live = day <= today;
    let endpoint = day == sel.0 || day == sel.1;
    let inside = day > sel.0 && day < sel.1;
    let mut cell = Border::new()
        .width(cw)
        .height(ch)
        .corner_radius(CornerRadius::uniform(4.0))
        // A null background never hit-tests — set an explicit transparent
        // brush so the whole cell still receives pointer input.
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)));
    if endpoint {
        cell = cell.background(Brush::Solid(rgba(accent, 1.0)));
    } else if inside {
        cell = cell.background(Brush::Solid(rgba(accent, 0.30)));
    } else if day == today {
        cell = cell
            .border_brush(Brush::Solid(rgba(theme.subtle_cf, 1.0)))
            .border_thickness(Thickness::uniform(1.0));
    }
    if live {
        cell = cell.on_pointer_pressed(ctx.callback(move |_| Msg::CalPick(y, m, d)));
    }
    cell.content(
        TextBlock::new()
            .text(d.to_string())
            .font_size(theme.body_size)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .foreground(if !live {
                theme.subtle
            } else if endpoint {
                Brush::Solid(Color::rgb(255, 255, 255))
            } else {
                theme.text
            }),
    )
}

/// Which month-navigation arrows a month grid carries — with two months
/// side by side only the outer edges get one.
#[derive(Clone, Copy)]
struct CalNav {
    prev: bool,
    next: bool,
    next_enabled: bool,
}

/// One month grid: header (‹ / › per `nav`), weekday row, six week rows.
fn cal_month(
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    month_first: jiff::civil::Date,
    sel: (jiff::civil::Date, jiff::civil::Date),
    today: jiff::civil::Date,
    size: (f64, f64),
    nav: CalNav,
) -> View {
    let (cw, ch) = size;
    let (y, m) = (month_first.year(), month_first.month());
    let arrow = |label: &'static str,
                 delta: i8,
                 shown: bool,
                 enabled: bool,
                 ctx: &mut ViewContext<Shell>|
     -> View {
        if !shown {
            return Border::new().width(cw).height(ch).into();
        }
        Button::new()
            .width(cw)
            .height(ch)
            .is_enabled(enabled)
            .on_click(ctx.callback(move |_| Msg::CalMonth(delta)))
            .content(label)
    };
    let head: View = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .children([
            arrow("‹", -1, nav.prev, true, ctx),
            Border::new().width(cw * 5.0).content(
                TextBlock::new()
                    .text(tf!("{} 年 {} 月", y, m))
                    .font_size(theme.body_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center),
            ),
            arrow("›", 1, nav.next, nav.next_enabled, ctx),
        ]);
    let weekdays: View = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .keyed_children(keyed(
            ["一", "二", "三", "四", "五", "六", "日"]
                .iter()
                .map(|w| -> View {
                    Border::new().width(cw).content(
                        TextBlock::new()
                            .text(tr(w))
                            .font_size(theme.label_size)
                            .foreground(theme.subtle)
                            .horizontal_alignment(HorizontalAlignment::Center),
                    )
                })
                .collect(),
        ));

    let lead = i64::from(month_first.weekday().to_monday_zero_offset());
    let mut rows: Vec<View> = vec![head, weekdays];
    for week in 0..6i64 {
        let cells: Vec<View> = (0..7i64)
            .map(|wd| {
                let n = week * 7 + wd - lead + 1;
                match month_first.checked_add(jiff::Span::new().days(n - 1)) {
                    Ok(day) if n >= 1 && day.month() == m => {
                        cal_day(theme, ctx, day, sel, today, size)
                    }
                    _ => Border::new().width(cw).height(ch).into(),
                }
            })
            .collect();
        rows.push(
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .keyed_children(keyed(cells)),
        );
    }
    vstack(2.0, rows)
}

/// Local wall-clock hour of an epoch-ms instant (system zone).
fn local_hour(ms: i64) -> i8 {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).hour())
        .unwrap_or(0)
}

/// Inclusive hour slots `(first, last)` a single-day window covers; a window
/// reaching the next local midnight ends at slot 23.
pub fn hour_span(day: jiff::civil::Date, start_ms: i64, end_ms: i64) -> (i8, i8) {
    use globaltokentracker_core::viewmodel::local_day_start;
    let midnight = day.tomorrow().map(local_day_start).unwrap_or(i64::MAX);
    let last = if end_ms >= midnight {
        23
    } else {
        local_hour(end_ms - 1)
    };
    (local_hour(start_ms), last)
}

/// Width tiers of the custom-range card (`cols` = `REFLOW_MIN_CELL` steps of
/// content width, 1..=`WIDTH_TIERS`). Each tier's row is sized to fit the
/// tier's narrowest width: month 7×cell, divider 49, info ≥ 392 (five
/// quick buttons), hour block ≈ 400.
#[derive(Clone, Copy, PartialEq)]
enum CalLayout {
    /// ≤2: calendar, info, hour block stacked.
    Stacked,
    /// 3–4: one month | info over hour block.
    Split,
    /// 5: one month | info | hour block in one row.
    Row,
    /// ≥6: two months | info | hour block.
    WideRow,
}

impl CalLayout {
    fn of(cols: usize) -> Self {
        match cols {
            0..=2 => Self::Stacked,
            3..=4 => Self::Split,
            5 => Self::Row,
            _ => Self::WideRow,
        }
    }
}

/// Day-cell size: tier 3 is the only one where 44 px cells would squeeze
/// the info column under its five quick buttons.
fn cal_cell(cols: usize) -> (f64, f64) {
    if cols == 3 {
        (40.0, 32.0)
    } else {
        (44.0, 34.0)
    }
}

/// `自定义` range chrome: inline month calendar(s) (click a start day, then an
/// end day), the resolved window with one-tap ranges, and a 24-hour dial that
/// narrows a single selected day to whole-hour slots. Everything is local
/// civil date/time — no WinRT date values, no UTC conversion.
fn custom_range_row(
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    start_ms: i64,
    end_ms: i64,
    cal: CalView,
    cols: usize,
    clock_dial: &w::ClockDialHandle,
) -> View {
    use globaltokentracker_core::viewmodel::local_date;
    let now = jiff::Zoned::now();
    let today = now.date();
    // end_ms is exclusive — the last INCLUDED day is the one before it.
    let (first, last) = (local_date(start_ms), local_date(end_ms - 1));
    let sel = (first, last);
    let days = first.until(last).map_or(1, |s| s.get_days() + 1);
    let (y, m) = cal.month;
    let shown = jiff::civil::Date::new(y, m, 1).unwrap_or(today.first_of_month());
    let at_current = (y, m) >= (today.year(), today.month());
    let size = cal_cell(cols);
    let layout = CalLayout::of(cols);

    let calendar: View = if layout == CalLayout::WideRow {
        // The shown month sits on the right so ‹/› stay at the outer edges
        // and a cross-month range reads in one glance.
        let prev = shown
            .checked_sub(jiff::Span::new().months(1))
            .unwrap_or(shown);
        let left = CalNav {
            prev: true,
            next: false,
            next_enabled: false,
        };
        let right = CalNav {
            prev: false,
            next: true,
            next_enabled: !at_current,
        };
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(24.0)
            .children([
                cal_month(theme, ctx, prev, sel, today, size, left),
                cal_month(theme, ctx, shown, sel, today, size, right),
            ])
    } else {
        let nav = CalNav {
            prev: true,
            next: true,
            next_enabled: !at_current,
        };
        cal_month(theme, ctx, shown, sel, today, size, nav)
    };

    // Hour slots of a single-day window; `None` for multi-day selections.
    let span = (first == last).then(|| hour_span(first, start_ms, end_ms));
    let partial = span.is_some_and(|s| s != (0, 23));
    let hhmm = |h: i8| format!("{h:02}:00");

    let hint = if cal.anchor.is_some() {
        t!("再点一个日期作为另一端")
    } else {
        t!("点击起始日，再点击结束日")
    };
    // One-tap ranges the top preset bar doesn't offer — a shared min width
    // so the row reads as one control.
    let quick: Vec<View> = ["昨天", "本周", "本月", "上月", "近 90 天"]
        .iter()
        .map(|k| {
            Button::new()
                .min_width(72.0)
                .on_click(ctx.callback(move |_| Msg::CalQuick(k)))
                .content(TextBlock::new().text(tr(k)).font_size(theme.label_size))
        })
        .collect();
    let quick_row = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .keyed_children(keyed(quick));
    let (headline, sub) = match span {
        Some((a, b)) if partial => (
            tf!("{} {} → {}", first, hhmm(a), hhmm(b + 1)),
            tf!("共 {} 小时 · {}", b - a + 1, hint),
        ),
        _ => (
            tf!("{} → {}", first, last),
            tf!("共 {} 天 · {}", days, hint),
        ),
    };
    let info: View = StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(6.0)
        .vertical_alignment(VerticalAlignment::Center)
        .children((
            TextBlock::new()
                .text(t!("已选区间"))
                .font_size(theme.label_size)
                .foreground(theme.subtle),
            TextBlock::new()
                .text(headline)
                .font_size(theme.body_size * 1.5)
                .font_weight(FontWeight::SEMI_BOLD),
            TextBlock::new()
                .text(sub)
                .font_size(theme.label_size)
                .foreground(theme.subtle),
            TextBlock::new()
                .text(t!("快捷选择"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .margin(Thickness::new(0.0, 14.0, 0.0, 2.0)),
            quick_row,
        ));

    // Hour block: dial + its caption column.
    let live_until = if first == today { now.hour() } else { 23 };
    let dial = w::clock_dial(theme, clock_dial, span, cal.clock_anchor, live_until, ctx);
    let dial_hint = match (span, cal.clock_anchor) {
        (None, _) => t!("选择单日后可按时段统计"),
        (Some(_), Some(_)) => t!("再点一个整点作为另一端"),
        (Some(_), None) => t!("点击起始整点，再点击结束整点"),
    };
    let dial_text = StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(6.0)
        .width(172.0)
        .vertical_alignment(VerticalAlignment::Center)
        .children((
            TextBlock::new()
                .text(t!("按时段"))
                .font_size(theme.label_size)
                .foreground(theme.subtle),
            TextBlock::new()
                .text(match span {
                    Some((a, b)) if partial => format!("{} – {}", hhmm(a), hhmm(b + 1)),
                    Some(_) => t!("全天").into(),
                    None => "—".into(),
                })
                .font_size(theme.body_size * 1.5)
                .font_weight(FontWeight::SEMI_BOLD),
            TextBlock::new()
                .text(dial_hint)
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap),
            Button::new()
                .is_enabled(partial)
                .margin(Thickness::new(0.0, 8.0, 0.0, 0.0))
                .on_click(ctx.callback(|_| Msg::ClockFullDay))
                .content(
                    TextBlock::new()
                        .text(t!("全天"))
                        .font_size(theme.label_size),
                ),
        ));
    let hours: View = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(16.0)
        .vertical_alignment(VerticalAlignment::Center)
        .children((dial, dial_text));

    let rule_v = || -> View {
        Border::new()
            .width(1.0)
            .margin(Thickness::xy(24.0, 0.0))
            .background(theme.divider)
            .into()
    };
    let rule_h = || -> View { Border::new().height(1.0).background(theme.divider).into() };
    let body: View = match layout {
        // One centred column under the calendar; info and the hour block
        // share a left edge inside it.
        CalLayout::Stacked => vstack(
            16.0,
            vec![
                Border::new()
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .content(calendar),
                rule_h(),
                Border::new()
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .content(vstack(16.0, vec![info, hours])),
            ],
        ),
        // Calendar pinned left; info over the hour block on the right.
        CalLayout::Split => Grid::new()
            .columns([GridLength::Auto, GridLength::Auto, GridLength::STAR])
            .children([
                cell(0, calendar),
                cell(1, rule_v()),
                cell(2, vstack(16.0, vec![info, rule_h(), hours])),
            ]),
        // Everything on one row: calendar(s) | info | hour block.
        CalLayout::Row | CalLayout::WideRow => Grid::new()
            .columns([
                GridLength::Auto,
                GridLength::Auto,
                GridLength::STAR,
                GridLength::Auto,
                GridLength::Auto,
            ])
            .children([
                cell(0, calendar),
                cell(1, rule_v()),
                cell(2, info),
                cell(3, rule_v()),
                cell(4, hours),
            ]),
    };
    w::card(theme, body)
}

fn page_frame(theme: &Theme, body: View) -> View {
    let bg = theme
        .page_bg
        .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground));
    let frame = Border::new()
        .padding(Thickness::xy(24.0, 16.0))
        .background(bg);
    ScrollViewer::new()
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .content(frame.content(body))
}

/// Assemble raw top-level blocks into the scrolled page — each block gets a
/// wrapper so it can ride its own compositor spring during a nav slide (rest
/// state uses the same wrappers, so settling never remounts).
pub fn frame_page(theme: &Theme, gap: f64, blocks: Vec<View>, slide: &Slide) -> View {
    page_frame(theme, vstack(gap, w::slide_children(blocks, slide)))
}

/// The workflow canvas measures against the viewport instead of its node extent.
pub fn bounded_frame_page(theme: &Theme, gap: f64, blocks: Vec<View>, slide: &Slide) -> View {
    ScrollViewer::new()
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .content(
            Border::new()
                .padding(Thickness::xy(24.0, 16.0))
                .background(
                    theme
                        .page_bg
                        .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground)),
                )
                .content(vstack(gap, w::slide_children(blocks, slide))),
        )
}

pub fn canvas_frame_page(theme: &Theme, _gap: f64, blocks: Vec<View>, slide: &Slide) -> View {
    Border::new()
        // Same page frame as every other page (16/12 made the workflow
        // toolbar start 8 DIP left of the titles on the other eight pages).
        .padding(Thickness::xy(24.0, 16.0))
        .background(
            theme
                .page_bg
                .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground)),
        )
        .content(
            Grid::new()
                .rows([GridLength::STAR])
                .keyed_children(keyed(w::slide_children(blocks, slide))),
        )
}

// ---------------------------------------------------------------- overview

fn edit_chrome(theme: &Theme, page: &str, id: &'static str, ctx: &mut ViewContext<Shell>) -> View {
    let page = page.to_string();
    let icon = w::widget_icon(id);
    let title = w::widget_title(id);
    Border::new()
        .background(theme.card_border)
        .padding(Thickness::xy(8.0, 3.0))
        .content(
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .children((
                    SymbolIcon::new().symbol(icon),
                    TextBlock::new()
                        .text(title)
                        .font_size(theme.label_size)
                        .vertical_alignment(VerticalAlignment::Center),
                    Button::new()
                        .on_click(ctx.callback({
                            let p = page.clone();
                            move |_| Msg::MoveWidget(p.clone(), id.to_string(), -1)
                        }))
                        .content(t!("上移")),
                    Button::new()
                        .on_click(ctx.callback({
                            let p = page.clone();
                            move |_| Msg::MoveWidget(p.clone(), id.to_string(), 1)
                        }))
                        .content(t!("下移")),
                    Button::new()
                        .on_click(ctx.callback({
                            let p = page.clone();
                            move |_| Msg::HideWidget(p.clone(), id.to_string(), true)
                        }))
                        .content(t!("隐藏")),
                )),
        )
}

fn hidden_chip(theme: &Theme, page: &str, id: &'static str, ctx: &mut ViewContext<Shell>) -> View {
    let page = page.to_string();
    Border::new().padding(Thickness::xy(10.0, 4.0)).content(
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                TextBlock::new()
                    .text(tf!("已隐藏：{}", w::widget_title(id)))
                    .font_size(theme.label_size)
                    .foreground(theme.subtle)
                    .vertical_alignment(VerticalAlignment::Center),
                Button::new()
                    .on_click(ctx.callback({
                        let p = page.clone();
                        move |_| Msg::HideWidget(p.clone(), id.to_string(), false)
                    }))
                    .content(t!("恢复")),
            )),
    )
}

/// Trend card header: title on the left, moving-average switch on the right.
fn trend_head(theme: &Theme, title: &str, on: bool, ctx: &mut ViewContext<Shell>) -> View {
    let caption = |on: bool| {
        TextBlock::new()
            .text(if on { t!("开") } else { t!("关") })
            .font_size(theme.label_size)
            .vertical_alignment(VerticalAlignment::Center)
    };
    // Same track slot as `pages::switch`: a Pixel column — the control's
    // 开/关 slot measures ~154 DIP while it draws ~44 (WinUI `width()` pins
    // the drawn track but not the reported desired size), so a StackPanel set
    // the caption ~110 DIP after the track and the group stopped short of the
    // card edge.
    let switch = Grid::new()
        .columns([GridLength::Auto, GridLength::Pixel(44.0), GridLength::Auto])
        .column_spacing(8.0)
        .children((
            TextBlock::new()
                .grid_column(0)
                .text(t!("折线趋势"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center),
            ToggleSwitch::new()
                .grid_column(1)
                .is_on(on)
                .on_toggled(ctx.callback(Msg::SetTrendLine))
                .width(44.0)
                .vertical_alignment(VerticalAlignment::Center)
                .slots([
                    SlotView::new(
                        ToggleSwitchSlot::OnContent,
                        TextBlock::new().text("").font_size(theme.label_size),
                    ),
                    SlotView::new(
                        ToggleSwitchSlot::OffContent,
                        TextBlock::new().text("").font_size(theme.label_size),
                    ),
                ]),
            caption(on).grid_column(2),
        ));
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children([
            cell(0, w::section_header(theme, Symbol::FourBars, title)),
            cell(1, switch),
        ])
}

/// Activity heatmap card: fixed ~1-year window (follows the tool/model
/// filters, not the range selector) with a 4-way metric switch.
fn heat_card(
    theme: &Theme,
    vm: &globaltokentracker_core::OverviewVm,
    args: &OverviewArgs,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let metric = HeatMetric::from_key(&args.config.heat_metric);
    let items: Vec<KeyedView> = HEAT_METRICS
        .iter()
        .map(|m| {
            KeyedView::new(
                m.key(),
                SelectorBarItem::new()
                    .text(tr(m.label()))
                    .is_selected(*m == metric),
            )
        })
        .collect();
    let selector: View = SelectorBar::new()
        .horizontal_alignment(HorizontalAlignment::Right)
        .on_selected_text_changed(ctx.callback(|t: Option<String>| {
            // Items carry only text — map the localized label back.
            let want = t.unwrap_or_default();
            let m = HEAT_METRICS
                .iter()
                .find(|m| tr(m.label()) == want)
                .copied()
                .unwrap_or_default();
            Msg::SetHeatMetric(m.key())
        }))
        .collection_slot(SelectorBarSlot::Items, items);
    let head: View = Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children([
            cell(
                0,
                w::section_header(theme, Symbol::Calendar, tr("活跃热力图")),
            ),
            cell(1, selector),
        ]);
    let mut col: Vec<View> = vec![
        head,
        w::heatmap(
            theme,
            &vm.heat,
            metric,
            args.heat,
            args.canvas_ready < 9,
            args.cols,
            ctx,
        ),
    ];
    if metric == HeatMetric::Duration {
        col.push(
            TextBlock::new()
                .text(t!(
                    "时长为调用耗时之和；Claude Code / Codex 为按日志时间戳推算"
                ))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .into(),
        );
    }
    w::card(
        theme,
        StackPanel::new()
            .orientation(Orientation::Vertical)
            .spacing(10.0)
            .keyed_children(keyed(col)),
    )
}

/// Render one overview widget (without edit chrome).
fn overview_widget(
    id: &str,
    s: &Snapshot,
    theme: &Theme,
    args: &OverviewArgs,
    ctx: &mut ViewContext<Shell>,
) -> Option<View> {
    let trend = args.trend;
    let vm = &s.vm;
    let rl = tr(vm.range.label());
    match id {
        // Reflow grid: columns stretch to the measured content width —
        // 4 across at normal width, 3/2/1 as the window narrows.
        "stats" => Some(reflow_grid(
            theme,
            vec![
                w::stat_card(
                    theme,
                    Symbol::Calculator,
                    &tf!("{rl} Tokens", rl),
                    fmt::tokens_exact(fmt::tokens_total(&vm.span)),
                    tf!("事件 {}", fmt::tokens_exact(vm.span.events)),
                    false,
                    None,
                ),
                w::stat_card(
                    theme,
                    Symbol::Tag,
                    &tf!("{rl}估算成本", rl),
                    fmt::usd(vm.span.cost_usd),
                    tf!("全部 {}", fmt::usd(vm.all.cost_usd)),
                    true,
                    Some((t!("估算"), w::BadgeTone::Accent)),
                ),
                w::stat_card(
                    theme,
                    Symbol::SyncFolder,
                    &tf!("{rl}缓存读", rl),
                    fmt::tokens_exact(vm.span.cache_read_tokens),
                    tf!("输入 {}", fmt::tokens_exact(vm.span.input_tokens)),
                    false,
                    None,
                ),
                w::stat_card(
                    theme,
                    Symbol::CalendarWeek,
                    &tf!("{rl}事件", rl),
                    fmt::tokens_exact(vm.span.events),
                    tf!(
                        "活跃 {}",
                        if vm.span.active_ms > 0 {
                            fmt::duration(Some(vm.span.active_ms as i64))
                        } else {
                            "—".into()
                        }
                    ),
                    false,
                    None,
                ),
            ],
            args.cols,
        )),
        "trend" => {
            let trend_title: String = match vm.range {
                Range::Today => t!("今日 · 按小时").into(),
                Range::All => t!("全部 · 按天（近 60 桶）").into(),
                Range::Custom { .. } if vm.range.hourly() => t!("自定义 · 按小时").into(),
                Range::Custom { .. } => t!("自定义 · 按天").into(),
                _ => tf!("{rl}趋势", rl),
            };
            Some(w::card(
                theme,
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .children((
                        trend_head(theme, &trend_title, args.config.trend_line, ctx),
                        w::trend_strip(
                            theme,
                            &vm.daily,
                            trend,
                            args.config.trend_line,
                            args.canvas_ready < 1,
                            args.cols,
                            ctx,
                        ),
                    )),
            ))
        }
        "heat" => Some(heat_card(theme, vm, args, ctx)),
        "share" => {
            /// Top-`keep` slices by value + 其他 fold; zero-value rows can't
            /// draw a wedge so they're dropped honestly before folding.
            fn fold(mut items: Vec<(String, f64)>, keep: usize) -> Vec<(String, f64)> {
                items.retain(|(_, v)| *v > 0.0);
                items.sort_by(|a, b| b.1.total_cmp(&a.1));
                let rest: f64 = items.iter().skip(keep).map(|i| i.1).sum();
                items.truncate(keep);
                if rest > 0.0 {
                    items.push((t!("其他").to_string(), rest));
                }
                items
            }
            let app_tok = |a: &globaltokentracker_core::store::AppSummary| {
                a.unclassified_tokens + a.input_tokens + a.output_tokens + a.cache_read_tokens
            };
            let mut columns = vec![
                (
                    tf!("{} · {}", tr("按工具"), tr("费用")),
                    fold(
                        vm.by_app
                            .iter()
                            .map(|a| (app_display(&a.app).to_string(), a.cost_usd))
                            .collect(),
                        4,
                    ),
                    fmt::usd as fn(f64) -> String,
                ),
                (
                    tf!("{} · {}", tr("按模型"), tr("费用")),
                    fold(
                        vm.by_model
                            .iter()
                            .map(|r| (r.name.clone(), r.cost_usd))
                            .collect(),
                        4,
                    ),
                    fmt::usd,
                ),
                (
                    tf!("{} · Tokens", tr("按工具")),
                    fold(
                        vm.by_app
                            .iter()
                            .map(|a| (app_display(&a.app).to_string(), app_tok(a) as f64))
                            .collect(),
                        4,
                    ),
                    |v| i18n::compact(v as u64),
                ),
                (
                    tf!("{} · Tokens", tr("按模型")),
                    fold(
                        vm.by_model
                            .iter()
                            .map(|r| (r.name.clone(), r.tokens as f64))
                            .collect(),
                        4,
                    ),
                    |v| i18n::compact(v as u64),
                ),
            ];
            // With paired devices: each device's part of the range — the
            // devices the Devices filter includes (and that are enabled).
            if vm.devices.len() > 1 {
                let shown: Vec<_> = vm
                    .devices
                    .iter()
                    .filter(|d| d.selected && d.enabled)
                    .collect();
                let label = |d: &globaltokentracker_core::viewmodel::DeviceVm| {
                    let name = if d.local {
                        crate::devices::local_label(&d.name)
                    } else {
                        d.name.clone()
                    };
                    if d.local && name != t!("本机") {
                        tf!("{}（本机）", name)
                    } else {
                        name
                    }
                };
                columns.push((
                    tf!("{} · {}", tr("按设备"), tr("费用")),
                    fold(
                        shown.iter().map(|d| (label(d), d.span.cost_usd)).collect(),
                        4,
                    ),
                    fmt::usd as fn(f64) -> String,
                ));
                columns.push((
                    tf!("{} · Tokens", tr("按设备")),
                    fold(
                        shown
                            .iter()
                            .map(|d| {
                                let t = &d.span;
                                let headline = t.unclassified_tokens
                                    + t.input_tokens
                                    + t.output_tokens
                                    + t.cache_read_tokens;
                                (label(d), headline as f64)
                            })
                            .collect(),
                        4,
                    ),
                    |v| i18n::compact(v as u64),
                ));
            }
            let mut cells: Vec<View> = Vec::with_capacity(columns.len());
            for (i, (title, slices, f)) in columns.iter().enumerate() {
                let total: f64 = slices.iter().map(|s| s.1).sum();
                let center = f(total);
                cells.push(w::donut_cell(
                    theme,
                    title.clone(),
                    w::DonutSpec {
                        slices,
                        center,
                        fmt_v: *f,
                        key: i as u8,
                        handle: &args.donuts[i],
                        // Trend takes stage 1, donut `i` stage 2+i (up to
                        // six with paired devices), the speed donut stage 8,
                        // the heatmap stage 9.
                        defer: args.canvas_ready < 2 + i,
                        wide: args.cols >= 5,
                    },
                    ctx,
                ));
            }
            // Donuts stretch across the measured width: 4→3→2→1 columns.
            Some(w::card(
                theme,
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .children((
                        w::section_header(theme, Symbol::AllApps, &tf!("{rl} · 占比分布", rl)),
                        reflow_grid(theme, cells, args.cols),
                    )),
            ))
        }
        // 模型速率卡：环形图按模型加权输出速度（Σout/Σms，非逐事件均值）
        // 分片，环心为总体均值；右侧 Top 榜按速率排序，行 tooltip 列示
        // 总/输入/输出/预填/解码分项。时长缺失的事件不计入（缺失≠零速），
        // 缺 duration_ms 但有首尾时间戳的行按有界 span 估算并以 ≈ 标注。
        "speed" => {
            let mut ranked: Vec<_> = vm.by_model.iter().filter(|r| r.speed_ms > 0).collect();
            ranked.sort_by(|a, b| {
                b.output_tps()
                    .unwrap_or(0.0)
                    .total_cmp(&a.output_tps().unwrap_or(0.0))
            });
            let max = ranked
                .first()
                .and_then(|r| r.output_tps())
                .unwrap_or(1.0)
                .max(1.0);
            let mut rows = vec![w::section_header(
                theme,
                Symbol::Go,
                &tf!("{rl} · 速度统计", rl),
            )];
            for r in ranked.iter().take(3) {
                rows.push(usage_row(
                    theme,
                    r.name.clone(),
                    r.output_tps().unwrap_or(0.0) / max * 100.0,
                    format!(
                        "{}{}",
                        if r.speed_est_ms > 0 { "≈" } else { "" },
                        fmt::speed(r.output_tps().unwrap_or(0.0))
                    ),
                ));
            }
            if ranked.is_empty() {
                rows.push(
                    TextBlock::new()
                        .text(t!("暂无数据"))
                        .foreground(theme.subtle)
                        .into(),
                );
            }
            rows.push(
                Button::new()
                    .on_click(ctx.callback(|_| Msg::Nav(Some("speed".into()))))
                    .content(t!("查看完整速度排行榜")),
            );
            Some(w::card(theme, vstack(10.0, rows)))
        }
        "apps" => {
            let tokens = |a: &globaltokentracker_core::store::AppSummary| {
                a.unclassified_tokens
                    + a.input_tokens
                    + a.output_tokens
                    + a.cache_read_tokens
                    + a.cache_write_tokens
            };
            let top = vm.by_app.iter().take(8).map(tokens).max().unwrap_or(0);
            let mut rows: Vec<View> = Vec::new();
            for a in vm.by_app.iter().take(8) {
                rows.push(usage_row(
                    theme,
                    tf!("{}  ·  {} 事件", app_display(&a.app), a.events),
                    if top > 0 {
                        tokens(a) as f64 / top as f64 * 100.0
                    } else {
                        0.0
                    },
                    tf!(
                        "{} tok  ·  {}",
                        fmt::tokens_exact(tokens(a)),
                        fmt::usd(a.cost_usd)
                    ),
                ));
            }
            if rows.is_empty() {
                rows.push(
                    TextBlock::new()
                        .text(t!("暂无数据"))
                        .font_size(theme.body_size)
                        .foreground(theme.subtle)
                        .into(),
                );
            }
            Some(w::card(
                theme,
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    // 10 like the trend/share/speed/heat cards — these three
                    // used 8, so the air under the section rule differed by
                    // 2 DIP from one card to the next on the same page.
                    .spacing(10.0)
                    .children((
                        w::section_header(theme, Symbol::List, &tf!("{rl} · 按工具", rl)),
                        vstack(2.0, rows),
                    )),
            ))
        }
        // Span totals per device for the range (this PC first). Only once
        // another device is paired — a lone PC has nothing to compare.
        "devices" if vm.devices.len() > 1 => {
            let top = vm
                .devices
                .iter()
                .map(|d| fmt::tokens_total(&d.span))
                .max()
                .unwrap_or(0);
            let mut rows: Vec<View> = Vec::new();
            for d in &vm.devices {
                let tokens = fmt::tokens_total(&d.span);
                let mut name = if d.local {
                    crate::devices::local_label(&d.name)
                } else {
                    d.name.clone()
                };
                if d.local && name != t!("本机") {
                    name = tf!("{}（本机）", name);
                }
                if !d.selected {
                    name = tf!("{} · 已筛除", name);
                }
                if d.last_error_code.is_some() {
                    name = format!("⚠ {name}");
                }
                rows.push(usage_row(
                    theme,
                    name,
                    if top > 0 {
                        tokens as f64 / top as f64 * 100.0
                    } else {
                        0.0
                    },
                    tf!(
                        "{} tok  ·  {}",
                        fmt::tokens_exact(tokens),
                        fmt::usd(d.span.cost_usd)
                    ),
                ));
            }
            if !vm.device_warnings.is_empty() {
                rows.push(
                    TextBlock::new()
                        .text(t!("部分设备可能重复计算了同一份用量——详见“设备”。"))
                        .font_size(theme.label_size)
                        .foreground(theme.warn)
                        .text_wrapping(TextWrapping::Wrap)
                        .into(),
                );
            }
            Some(w::card(
                theme,
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .children((
                        w::section_header(theme, Symbol::Remote, &tf!("{rl} · 按设备", rl)),
                        vstack(2.0, rows),
                    )),
            ))
        }
        "devices" => None,
        "quotas" => {
            // Most pressing first: windows still running before expired ones,
            // then by used percent (unknown last). Stable, so ties keep the
            // store's app/window order.
            let now = globaltokentracker_core::store::now_ms();
            let live =
                |q: &globaltokentracker_core::store::QuotaRow| q.resets_at.is_none_or(|t| t >= now);
            let mut ranked: Vec<&globaltokentracker_core::store::QuotaRow> =
                vm.quotas.iter().collect();
            ranked.sort_by(|a, b| {
                live(b).cmp(&live(a)).then_with(|| {
                    b.used_percent
                        .partial_cmp(&a.used_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            });
            let mut rows: Vec<View> = ranked
                .iter()
                .take(QUOTA_STRIP_ROWS)
                .map(|q| quota_strip_row(theme, q))
                .collect();
            if ranked.len() > QUOTA_STRIP_ROWS {
                rows.push(
                    TextBlock::new()
                        .text(tf!(
                            "另有 {} 项 · 见配额页",
                            ranked.len() - QUOTA_STRIP_ROWS
                        ))
                        .font_size(theme.label_size)
                        .foreground(theme.subtle)
                        .into(),
                );
            }
            if rows.is_empty() {
                rows.push(
                    TextBlock::new()
                        .text(t!("暂无配额信号"))
                        .font_size(theme.body_size)
                        .foreground(theme.subtle)
                        .into(),
                );
            }
            Some(w::card(
                theme,
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .children((
                        w::section_header(theme, Symbol::Clock, t!("订阅配额")),
                        vstack(2.0, rows),
                    )),
            ))
        }
        "unpriced" => {
            if vm.unpriced.is_empty() {
                return None;
            }
            Some(
                InfoBar::new()
                    .severity(InfoBarSeverity::Warning)
                    .is_open(true)
                    .title(t!("未计价模型").to_string())
                    .message(tf!(
                        "{} — 请在价格页补充覆写",
                        vm.unpriced
                            .iter()
                            .take(6)
                            .map(|(m, n)| format!("{m}×{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .into(),
            )
        }
        _ => None,
    }
}

/// Chrome-strip geometry — labels and pills are width-pinned so the overlay
/// panel can sit under its button without measuring. Strip order: 工具 → 模型
/// → 刷新 (data filters first, the cadence setting last).
const CHROME_LEFT: f64 = 24.0;
const GROUP_GAP: f64 = 8.0;
const PICKER_GAP: f64 = 20.0;

/// Pinned widths + the panel x under each pill. They follow the language and
/// the body font: "Refresh"/"Models" and "12/15 selected" are wider than
/// 刷新/模型 and 已选 12/15, and a larger font widens both — fixed 28/104/120
/// clipped the labels to "Refres" and "Mode".
struct ChromeGeom {
    label_w: f64,
    tools_w: f64,
    models_w: f64,
    devices_w: f64,
    refresh_w: f64,
    tools_x: f64,
    models_x: f64,
    devices_x: f64,
    refresh_x: f64,
}

/// `with_devices`: the device picker sits between Models and Refresh once
/// another device is paired.
fn chrome_geom(theme: &Theme, with_devices: bool) -> ChromeGeom {
    let en = i18n::lang() == i18n::Lang::En;
    // Design widths are for the default 12pt body; only ever grow from there.
    let k = (theme.body_size / 12.0).max(1.0);
    let label_w = if en {
        (theme.label_size * 4.4).ceil()
    } else {
        (theme.label_size * 2.0 + 6.0).ceil()
    };
    let (pill_w, refresh_w) = if en {
        (140.0 * k, 168.0 * k)
    } else {
        (104.0 * k, 120.0 * k)
    };
    let tools_x = CHROME_LEFT + label_w + GROUP_GAP;
    let models_x = tools_x + pill_w + PICKER_GAP + label_w + GROUP_GAP;
    let devices_x = models_x + pill_w + PICKER_GAP + label_w + GROUP_GAP;
    let refresh_x = if with_devices {
        devices_x + pill_w + PICKER_GAP + label_w + GROUP_GAP
    } else {
        devices_x
    };
    ChromeGeom {
        label_w,
        tools_w: pill_w,
        models_w: pill_w,
        devices_w: pill_w,
        refresh_w,
        tools_x,
        models_x,
        devices_x,
        refresh_x,
    }
}

/// Filter-strip state bundle — keeps `filter_chrome`/`dropdown_overlay`
/// signatures tidy as more pickers join.
pub struct ChromeState<'a> {
    pub apps: &'a Option<Vec<String>>,
    pub models: &'a Option<Vec<String>>,
    /// LAN device scope; the picker shows once `vm.devices` has a peer.
    pub devices: &'a Option<Vec<String>>,
    pub refresh_secs: u64,
    pub open: Option<MenuKind>,
}

/// Pill summary for a multi-select checklist: `None` = all, `Some([])` =
/// deliberately empty, `Some(f)` = partial coverage.
fn check_summary(filter: &Option<Vec<String>>, total: usize) -> String {
    match filter {
        None => t!("全部").to_string(),
        Some(f) if f.is_empty() => t!("未选").into(),
        Some(f) => tf!("已选 {}/{total}", f.len(), total),
    }
}

/// Filter strip shared by overview + detail: tool-scope, model-scope, and
/// refresh-cadence pickers on one row that cannot overflow. Buttons toggle an
/// in-content overlay (`dropdown_overlay`) instead of a system Flyout — see
/// `Shell::open_menu`.
pub fn filter_chrome(
    s: &Snapshot,
    theme: &Theme,
    state: &ChromeState,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let with_devices = s.vm.devices.len() > 1;
    let g = chrome_geom(theme, with_devices);
    let mut pickers: Vec<View> = vec![
        picker_button(
            theme,
            check_summary(state.apps, s.vm.apps.len()),
            (g.label_w, g.tools_w),
            state.open == Some(MenuKind::Tools),
            MenuKind::Tools,
            ctx,
        ),
        picker_button(
            theme,
            check_summary(state.models, s.vm.models.len()),
            (g.label_w, g.models_w),
            state.open == Some(MenuKind::Models),
            MenuKind::Models,
            ctx,
        ),
    ];
    if with_devices {
        pickers.push(picker_button(
            theme,
            check_summary(state.devices, s.vm.devices.len()),
            (g.label_w, g.devices_w),
            state.open == Some(MenuKind::Devices),
            MenuKind::Devices,
            ctx,
        ));
    }
    pickers.push(picker_button(
        theme,
        tr(refresh_label(state.refresh_secs)).to_string(),
        (g.label_w, g.refresh_w),
        state.open == Some(MenuKind::Refresh),
        MenuKind::Refresh,
        ctx,
    ));
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(PICKER_GAP)
        // 4 DIP sat the pills right on the nav's hairline; the page below
        // resumes with 16, so the strip reads as its own band at 8.
        .margin(Thickness::xy(CHROME_LEFT, 8.0))
        .keyed_children(keyed(pickers))
}

/// Label + fixed-width pill whose click toggles `kind` in `open_menu`.
/// The chevron flips while the panel is open.
fn picker_button(
    theme: &Theme,
    value: String,
    (label_w, width): (f64, f64),
    open: bool,
    kind: MenuKind,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let (label, a11y) = match kind {
        MenuKind::Tools => (t!("工具"), t!("工具筛选")),
        MenuKind::Models => (t!("模型"), t!("模型筛选")),
        MenuKind::Devices => (t!("设备"), t!("设备筛选")),
        MenuKind::Refresh => (t!("刷新"), t!("刷新频率")),
    };
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(GROUP_GAP)
        .children((
            // The label sits above the click-away backdrop, so it dismisses an
            // open picker itself (transparent fill keeps it hit-testable).
            Border::new()
                .width(label_w)
                .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
                .on_pointer_pressed(ctx.callback(|_: PointerEventInfo| Msg::CloseMenu))
                .content(
                    TextBlock::new()
                        .text(label)
                        .font_size(theme.label_size)
                        .foreground(theme.subtle)
                        .vertical_alignment(VerticalAlignment::Center),
                ),
            Button::new()
                .automation_name(a11y)
                .width(width)
                .on_click(ctx.callback(move |_| Msg::ToggleMenu(kind)))
                .content(
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(6.0)
                        .children((
                            TextBlock::new()
                                .text(value)
                                .font_size(theme.body_size)
                                .vertical_alignment(VerticalAlignment::Center),
                            TextBlock::new()
                                .text(if open { "▴" } else { "▾" })
                                .font_size(theme.label_size)
                                .foreground(theme.subtle)
                                .vertical_alignment(VerticalAlignment::Center),
                        )),
                ),
        ))
}

/// Tool-checkbox rows plus the select-all / clear footer — extracted from the
/// old flyout so the overlay and chrome stay in sync. `filter` `None` = all
/// checked; `Some(vec![])` = deliberately empty view.
fn tools_menu_items(
    s: &Snapshot,
    theme: &Theme,
    filter: &Option<Vec<String>>,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    let mut checks: Vec<View> = Vec::with_capacity(s.vm.apps.len());
    for name in &s.vm.apps {
        let checked = filter.as_ref().is_none_or(|f| f.contains(name));
        let n = name.clone();
        checks.push(
            CheckBox::new()
                .is_checked(checked)
                .on_is_checked_changed(ctx.callback(move |on: bool| Msg::ToggleApp(n.clone(), on)))
                .content(
                    TextBlock::new()
                        .text(name.clone())
                        .font_size(theme.body_size),
                ),
        );
    }
    // Same shape as the model picker: the list scrolls inside a cap so the
    // select-all/clear footer stays visible and the panel never runs past the
    // window edge (the tools list had no cap at all).
    vec![
        ScrollViewer::new()
            .max_height(300.0)
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .content(
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .keyed_children(keyed(checks)),
            ),
        Border::new()
            .height(1.0)
            .background(theme.divider)
            .margin(Thickness::xy(0.0, 6.0))
            .into(),
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetApps(None)))
                    .content(t!("全选")),
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetApps(Some(Vec::new()))))
                    .content(t!("清空")),
            )),
    ]
}

/// Model-checkbox rows plus the select-all / clear footer. The list is
/// capped + scrollable — dozens of distinct models land here and a fixed
/// height keeps the card from running off the window. `s.vm.models` is
/// already scoped by the tool filter (cascade), not by the model filter.
fn models_menu_items(
    s: &Snapshot,
    theme: &Theme,
    filter: &Option<Vec<String>>,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    let mut checks: Vec<View> = Vec::with_capacity(s.vm.models.len());
    for name in &s.vm.models {
        let checked = filter.as_ref().is_none_or(|f| f.contains(name));
        let n = name.clone();
        checks.push(
            CheckBox::new()
                .is_checked(checked)
                .on_is_checked_changed(
                    ctx.callback(move |on: bool| Msg::ToggleModel(n.clone(), on)),
                )
                .content(
                    TextBlock::new()
                        .text(truncate(name, 36))
                        .font_size(theme.body_size)
                        .tooltip(name.clone()),
                ),
        );
    }
    vec![
        ScrollViewer::new()
            .max_height(300.0)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .content(
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(10.0)
                    .keyed_children(keyed(checks)),
            ),
        Border::new()
            .height(1.0)
            .background(theme.divider)
            .margin(Thickness::xy(0.0, 6.0))
            .into(),
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetModels(None)))
                    .content(t!("全选")),
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetModels(Some(Vec::new()))))
                    .content(t!("清空")),
            )),
    ]
}

/// Device-checkbox rows (this PC first) plus select-all / clear — the LAN
/// device dimension. `filter` `None` = all devices.
fn devices_menu_items(
    s: &Snapshot,
    theme: &Theme,
    filter: &Option<Vec<String>>,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    let mut col: Vec<View> = Vec::with_capacity(s.vm.devices.len() + 2);
    for d in &s.vm.devices {
        let checked = filter.as_ref().is_none_or(|f| f.contains(&d.device_id));
        let id = d.device_id.clone();
        let label = if d.local {
            let name = crate::devices::local_label(&d.name);
            if name == t!("本机") {
                name
            } else {
                tf!("{}（本机）", name)
            }
        } else {
            d.name.clone()
        };
        col.push(
            CheckBox::new()
                .is_checked(checked)
                .on_is_checked_changed(
                    ctx.callback(move |on: bool| Msg::ToggleDevice(id.clone(), on)),
                )
                .content(TextBlock::new().text(label).font_size(theme.body_size)),
        );
    }
    col.push(
        Border::new()
            .height(1.0)
            .background(theme.divider)
            .margin(Thickness::xy(0.0, 6.0))
            .into(),
    );
    col.push(
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetDevices(None)))
                    .content(t!("全选")),
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetDevices(Some(Vec::new()))))
                    .content(t!("清空")),
            )),
    );
    col
}

/// Refresh-cadence radio rows — single-select, so a pick light-dismisses the
/// panel (`SetRefreshSecs` clears `open_menu`). `0` seconds = file-watch only.
fn refresh_menu_items(theme: &Theme, secs: u64, ctx: &mut ViewContext<Shell>) -> Vec<View> {
    let mut rows: Vec<View> = Vec::with_capacity(REFRESH_OPTIONS.len());
    for (s, label) in REFRESH_OPTIONS {
        rows.push(
            Border::new()
                .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
                .on_pointer_pressed(ctx.callback(move |_: PointerEventInfo| Msg::SetRefreshSecs(s)))
                .content(
                    RadioButton::new()
                        .group_name("refresh-cadence")
                        .is_checked(s == secs)
                        .on_checked(ctx.callback(move |on: bool| {
                            if on {
                                Msg::SetRefreshSecs(s)
                            } else {
                                Msg::Noop
                            }
                        }))
                        .content(TextBlock::new().text(tr(label)).font_size(theme.body_size)),
                ),
        );
    }
    rows
}

/// In-content dropdown layer, rendered as the last child of the content cell
/// so it paints over the page. The card carries our own border + background —
/// the system FlyoutPresenter (which draws the pale surface stroke that read
/// as a bright halo over dark chrome) is gone entirely. A full-area
/// transparent border underneath swallows outside clicks to light-dismiss;
/// the trigger row above stays live (overlay only covers the content row).
pub fn dropdown_overlay(
    s: &Snapshot,
    theme: &Theme,
    state: &ChromeState,
    open: MenuKind,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let g = chrome_geom(theme, s.vm.devices.len() > 1);
    let (x, title, items) = match open {
        MenuKind::Tools => (
            g.tools_x,
            t!("工具筛选"),
            tools_menu_items(s, theme, state.apps, ctx),
        ),
        MenuKind::Models => (
            g.models_x,
            t!("模型筛选"),
            models_menu_items(s, theme, state.models, ctx),
        ),
        MenuKind::Devices => (
            g.devices_x,
            t!("设备筛选"),
            devices_menu_items(s, theme, state.devices, ctx),
        ),
        MenuKind::Refresh => (
            g.refresh_x,
            t!("刷新频率"),
            refresh_menu_items(theme, state.refresh_secs, ctx),
        ),
    };
    // Margin positions the card inside the content cell — plain Grid, no
    // Canvas (its empty-area hits can swallow presses meant for siblings).
    let close = Button::new()
        .on_click(ctx.callback(|_| Msg::CloseMenu))
        .content(t!("收起"));
    Grid::new()
        .grid_row(3)
        .horizontal_alignment(HorizontalAlignment::Left)
        .vertical_alignment(VerticalAlignment::Top)
        .margin(Thickness::new(x, 4.0, 0.0, 0.0))
        .children([w::overlay_panel(
            theme,
            title,
            close,
            StackPanel::new()
                .orientation(Orientation::Vertical)
                .spacing(10.0)
                .keyed_children(keyed(items)),
        )])
}

/// Bundle for the overview page — keeps the signature under the arg limit
/// and makes the shell→page handoff explicit.
pub struct OverviewArgs<'a> {
    pub scanning: bool,
    pub config: &'a UiConfig,
    pub editing: bool,
    pub trend: &'a w::TrendHandle,
    pub heat: &'a w::HeatHandle,
    pub clock_dial: &'a w::ClockDialHandle,
    /// Width tier from the ruler: `REFLOW_MIN_CELL` steps, 1..=`WIDTH_TIERS`
    /// (reflow grids cap it at `REFLOW_MAX_COLS`).
    pub cols: usize,
    /// Bound to the 1-DIP ruler panel mounted on this page.
    pub ruler: &'a ElementRef<SwapChainPanel>,
    /// Hover state per share-donut column plus the speed donut (index 6)
    /// (pointer → Msg → invalidation).
    pub donuts: &'a [w::DonutHandle; 7],
    /// How many of the page's D2D charts may be mounted yet (trend = 1st,
    /// then one donut per stage). Held at 0 while a slide starts, then
    /// stepped up one per ~16ms from the slide's back half
    /// (`Msg::CanvasStage`); the rest of the time it sits at `usize::MAX`.
    /// Anything not yet allowed renders as a same-size placeholder
    /// (`widgets::defer_slot`).
    pub canvas_ready: usize,
    /// Custom-range calendar state (month on show, pending first click).
    pub cal: CalView,
}

/// Returns the page's raw top-level blocks — `frame_page` assembles them
/// (gap + spring wrappers) so nav flights can cache the vec once.
pub fn overview_page(
    snap: Option<&Snapshot>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    args: &OverviewArgs,
) -> Vec<View> {
    let (config, editing) = (args.config, args.editing);
    let Some(s) = snap else {
        return vec![loading(theme, args.scanning)];
    };
    let scanning = args.scanning;

    let registry = w::registry_ids();
    let order = config.order_for("overview", &registry);
    let hidden = config.hidden("overview");

    let range_item = |label: &'static str, r: Range| {
        SelectorBarItem::new()
            .text(tr(label))
            .is_selected(s.vm.range == r)
    };
    let is_custom = matches!(s.vm.range, Range::Custom { .. });
    let range_sel: View = SelectorBar::new()
        .on_selected_text_changed(ctx.callback(|t: Option<String>| {
            // SelectorBarItem carries only text — map the localized label
            // back to the semantic value (works in either UI language).
            let want = t.unwrap_or_default();
            if want == tr("自定义") {
                return Msg::PickCustomRange;
            }
            let r = Range::LIST
                .iter()
                .find(|r| tr(r.label()) == want)
                .copied()
                .unwrap_or_default();
            Msg::SetRange(r)
        }))
        .collection_slot(
            SelectorBarSlot::Items,
            [
                KeyedView::new("today", range_item("今日", Range::Today)),
                KeyedView::new("week", range_item("近 7 天", Range::Week)),
                KeyedView::new("month", range_item("近 30 天", Range::Month)),
                KeyedView::new("all", range_item("全部", Range::All)),
                KeyedView::new(
                    "custom",
                    SelectorBarItem::new()
                        .text(tr("自定义"))
                        .is_selected(is_custom),
                ),
            ],
        );

    let head = header(
        theme,
        t!("总览"),
        vec![
            range_sel,
            if scanning {
                ProgressRing::new()
                    .is_indeterminate(true)
                    .is_active(true)
                    .width(18.0)
                    .height(18.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .into()
            } else {
                // Same footprint as the ring: a zero-width stand-in made the
                // 布局/刷新 buttons slide 28 DIP whenever a scan started or
                // finished.
                Border::new().width(18.0).height(18.0).into()
            },
            Button::new()
                .on_click(ctx.callback(|_| Msg::ToggleEdit))
                .content(if editing { t!("完成") } else { t!("布局") }),
            Button::new()
                .on_click(ctx.callback(|_| Msg::Rescan))
                .content(t!("刷新")),
        ]
        .into_iter()
        .collect(),
    );

    // 1-DIP full-width ruler: its surface Metrics report the real content
    // width → column-count changes are quantized here so a resize drag
    // only rebuilds when a column boundary is crossed.
    let on_width = ctx.callback(|cols: usize| Msg::SetOverviewCols(cols));
    ctx.use_effect("overview-ruler", (), {
        let ruler = args.ruler.clone();
        move || {
            let last = std::cell::Cell::new(0usize);
            let obs = ruler.observe_surface(move |event| {
                if let SwapChainPanelEvent::Metrics { width, .. } = event {
                    let cols = fit_cols(width, REFLOW_MIN_CELL, 12.0, WIDTH_TIERS);
                    if cols != last.get() {
                        last.set(cols);
                        let _ = on_width.call(cols);
                    }
                }
            });
            Some(Box::new(move || drop(obs)))
        }
    });
    // The ruler rides inside the title block: as a block of its own it cost a
    // full section gap above and below a 1-DIP line nobody can see.
    let mut col: Vec<View> = vec![
        Grid::new()
            .rows([GridLength::Auto, GridLength::Auto])
            .children((
                head,
                SwapChainPanel::new().element_ref(args.ruler).height(1.0),
            )),
    ];

    if let Range::Custom { start_ms, end_ms } = s.vm.range {
        col.push(custom_range_row(
            theme,
            ctx,
            start_ms,
            end_ms,
            args.cal,
            args.cols,
            args.clock_dial,
        ));
    }

    for id in &order {
        let id_static: &'static str = match registry.iter().find(|r| **r == id) {
            Some(r) => r,
            None => continue,
        };
        if hidden.contains(id) {
            if editing {
                col.push(hidden_chip(theme, "overview", id_static, ctx));
            }
            continue;
        }
        if let Some(v) = overview_widget(id_static, s, theme, args, ctx) {
            if editing {
                col.push(vstack(
                    4.0,
                    vec![edit_chrome(theme, "overview", id_static, ctx), v],
                ));
            } else {
                col.push(v);
            }
        }
    }

    col
}

// ---------------------------------------------------------------- detail

/// One column layout of a virtualized table: (logical column, DIP width at the
/// design 12pt body size — `None` = the star column that takes the rest).
/// Header and every row build from the same plan, so identical widths on each
/// per-row Grid keep the columns aligned without one giant 200-row measure.
type Plan = &'static [(usize, Option<f64>)];

/// Horizontal padding inside a table row (10 + 10).
const ROW_PAD: f64 = 20.0;
/// What the star (model) column must keep before a plan counts as fitting.
const STAR_MIN: f64 = 140.0;

/// Fixed columns hold numbers, which grow with the body font.
fn font_scale(theme: &Theme) -> f64 {
    (theme.body_size / 12.0).max(1.0)
}

/// The widest plan whose fixed columns leave `STAR_MIN` for the star column at
/// this row width and font scale; the narrowest when none does. A row width
/// of 0 (ruler not measured yet) takes the widest.
fn pick_plan(plans: &[Plan], width: f64, k: f64) -> Plan {
    if width <= 0.0 {
        return plans[0];
    }
    plans
        .iter()
        .copied()
        .find(|p| {
            let fixed: f64 = p.iter().filter_map(|(_, w)| *w).sum();
            fixed * k + ROW_PAD + STAR_MIN <= width
        })
        .unwrap_or(plans[plans.len() - 1])
}

/// Grid columns of a plan plus the physical index of each of the table's `N`
/// logical columns (`None` = left out by this plan).
fn plan_layout<const N: usize>(plan: Plan, k: f64) -> (Vec<GridLength>, [Option<i32>; N]) {
    let mut at = [None; N];
    let mut cols = Vec::with_capacity(plan.len());
    for (i, (logical, w)) in plan.iter().enumerate() {
        at[*logical] = Some(i as i32);
        cols.push(w.map_or(GridLength::STAR, |v| GridLength::Pixel(v * k)));
    }
    (cols, at)
}

/// Detail plans over the logical columns time, tool, model, input, output,
/// cache, cost, duration, speed — as the window narrows the (subtle) cache
/// column goes first, then duration, then the tool, then speed.
const DETAIL_PLANS: [Plan; 5] = [
    &[
        (0, Some(90.0)),
        (1, Some(96.0)),
        (2, None),
        (3, Some(88.0)),
        (4, Some(88.0)),
        (5, Some(88.0)),
        (6, Some(92.0)),
        (7, Some(64.0)),
        (8, Some(78.0)),
    ],
    &[
        (0, Some(84.0)),
        (1, Some(88.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
        (7, Some(60.0)),
        (8, Some(72.0)),
    ],
    &[
        (0, Some(84.0)),
        (1, Some(88.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
        (8, Some(72.0)),
    ],
    &[
        (0, Some(84.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
        (8, Some(72.0)),
    ],
    &[
        (0, Some(84.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
    ],
];

/// Wide-window plan for high-resolution displays (width >= ~1350 DIP):
/// expands time, tool, numeric metrics and duration so wide displays don't
/// allocate 75%+ of the screen to just the model column while numbers crowd.
pub const DETAIL_PLAN_WIDE: Plan = &[
    (0, Some(136.0)),
    (1, Some(116.0)),
    (2, None),
    (3, Some(108.0)),
    (4, Some(108.0)),
    (5, Some(108.0)),
    (6, Some(112.0)),
    (7, Some(80.0)),
    (8, Some(96.0)),
];

fn detail_layout(theme: &Theme, width: f64) -> (Vec<GridLength>, [Option<i32>; 9]) {
    let k = font_scale(theme);
    let plan = if width >= 1350.0 * k {
        DETAIL_PLAN_WIDE
    } else {
        pick_plan(&DETAIL_PLANS, width, k)
    };
    plan_layout(plan, k)
}

fn dcell(col: i32, v: View) -> View {
    Border::new().grid_column(col).content(v)
}

fn dtext(theme: &Theme, text: String, right: bool) -> TextBlock {
    // Ellipsis instead of a hard clip when a value outgrows its column.
    let t = TextBlock::new()
        .text(text)
        .font_size(theme.body_size)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .vertical_alignment(VerticalAlignment::Center);
    if right {
        t.horizontal_alignment(HorizontalAlignment::Right)
    } else {
        t.horizontal_alignment(HorizontalAlignment::Left)
    }
}

fn detail_header(theme: &Theme, width: f64) -> View {
    let (cols, at) = detail_layout(theme, width);
    let h = |text: &str, logical: usize, right: bool| -> Option<View> {
        at[logical].map(|col| {
            dcell(
                col,
                dtext(theme, text.into(), right)
                    .font_size(theme.label_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(theme.subtle)
                    .into(),
            )
        })
    };
    let cells: Vec<View> = [
        h(t!("时间"), 0, false),
        h(t!("工具"), 1, false),
        h(t!("模型"), 2, false),
        h(t!("输入"), 3, true),
        h(t!("输出"), 4, true),
        h(t!("缓存"), 5, true),
        h(t!("成本"), 6, true),
        h(t!("时长"), 7, true),
        h(t!("速度"), 8, true),
    ]
    .into_iter()
    .flatten()
    .collect();
    Border::new()
        .padding(Thickness::xy(10.0, 6.0))
        .border_brush(theme.divider)
        .border_thickness(Thickness::new(0.0, 0.0, 0.0, 1.0))
        .content(Grid::new().columns(cols).keyed_children(keyed(cells)))
}

fn event_row(theme: &Theme, r: &EventRow, zebra: bool, width: f64) -> View {
    let (cols, at) = detail_layout(theme, width);
    let model = r
        .model
        .clone()
        .or_else(|| r.pricing_model.clone())
        .unwrap_or_else(|| "—".into());
    let credits_text = r
        .credits
        .filter(|c| *c > 0.0)
        .map(|c| format!("{}cr", fmt::trim_f(c, 1)));
    // On unpriced rows the stored $0 carries no information; credit-billed
    // tools (Qoder) report their real billing in credits, so show that first.
    let mut cost = if r.cost_source.as_deref() == Some("unpriced") {
        credits_text
            .clone()
            .or_else(|| r.cost_usd.map(fmt::usd))
            .unwrap_or_else(|| "—".into())
    } else {
        r.cost_usd
            .map(fmt::usd)
            .or_else(|| credits_text.clone())
            .unwrap_or_else(|| "—".into())
    };
    match r.cost_source.as_deref() {
        Some("estimated") => cost.push_str(" ≈"),
        Some("provider_reported") => cost.push_str(" ↺"),
        _ => {}
    }
    if r.unclassified_tokens > 0 {
        cost = format!(
            "{} · {} tokens",
            tr("未分类"),
            fmt::tokens_exact(r.unclassified_tokens)
        );
    }
    let mut cost_children: Vec<View> = vec![dtext(theme, cost, true).into()];
    if r.cost_source.as_deref() == Some("unpriced") {
        cost_children.push(w::badge(theme, "unpriced".into(), w::BadgeTone::Warn));
    }
    let put = |logical: usize, v: View| at[logical].map(|col| dcell(col, v));
    let cells: Vec<View> = [
        put(0, dtext(theme, fmt::ts_short(r.ts_start), false).into()),
        put(
            1,
            dtext(theme, app_display(&r.app).to_string(), false).into(),
        ),
        put(
            2,
            dtext(theme, model, false).foreground(theme.subtle).into(),
        ),
        put(
            3,
            dtext(theme, fmt::tokens_exact(r.input_tokens), true).into(),
        ),
        put(
            4,
            dtext(theme, fmt::tokens_exact(r.output_tokens), true).into(),
        ),
        put(
            5,
            dtext(
                theme,
                fmt::tokens_exact(r.cache_read_tokens + r.cache_write_tokens),
                true,
            )
            .foreground(theme.subtle)
            .into(),
        ),
        put(
            6,
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(6.0)
                .horizontal_alignment(HorizontalAlignment::Right)
                .keyed_children(keyed(cost_children)),
        ),
        put(7, dtext(theme, fmt::duration(r.duration_ms), true).into()),
        put(
            8,
            match r.output_tps() {
                // ≈ marks timestamp-derived timing (no measured duration).
                Some((v, est)) => dtext(
                    theme,
                    format!("{}{}", fmt::speed(v), if est { " ≈" } else { "" }),
                    true,
                )
                .into(),
                None => dtext(theme, "—".into(), true).into(),
            },
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut row = Border::new().padding(Thickness::xy(10.0, 5.0));
    if theme.line_separators {
        row = row
            .border_brush(theme.divider)
            .border_thickness(Thickness::new(0.0, 0.0, 0.0, 1.0));
    }
    if zebra {
        // ~4% gray reads on both light and dark Fluent surfaces.
        row = row.background(Brush::Solid(Color::argb(10, 128, 128, 128)));
    }
    // One tooltip per row — windows-reactor rejects a tooltip nested inside
    // another tooltip's target subtree, so the rate breakdown rides on the
    // provenance tooltip as a second line.
    row.content(Grid::new().columns(cols).keyed_children(keyed(cells)))
        .tooltip(row_tip(r))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

/// One model's rate breakdown for hover text: total / input / output over
/// the timed span, plus prefill (input/TTFT) and decode (output/(timed−TTFT))
/// when reported; a tail note when some of the span was timestamp-estimated.
fn speed_tip(r: &globaltokentracker_core::store::ShareRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = r.total_tps() {
        parts.push(tf!("总 {}", fmt::speed(v)));
    }
    if let Some(v) = r.input_tps() {
        parts.push(tf!("输入 {}", fmt::speed(v)));
    }
    if let Some(v) = r.output_tps() {
        parts.push(tf!("输出 {}", fmt::speed(v)));
    }
    if let Some(v) = r.prefill_tps() {
        parts.push(tf!("预填 {}", fmt::speed(v)));
    }
    if let Some(v) = r.decode_tps() {
        parts.push(tf!("解码 {}", fmt::speed(v)));
    }
    if r.speed_est_ms > 0 {
        parts.push(t!("含首尾时间戳估算").to_string());
    }
    parts.join(" · ")
}

/// Row tooltip = provenance (`raw_ref`) plus the per-request rate breakdown
/// on a second line — the row is the only tooltip level (nested tooltips
/// inside a tooltip's target are rejected by windows-reactor).
fn row_tip(r: &EventRow) -> String {
    let speed = event_speed_tip(r);
    let prov = r.raw_ref.clone().unwrap_or_default();
    if speed.is_empty() {
        return prov;
    }
    if prov.is_empty() {
        return speed;
    }
    format!("{prov}\n{speed}")
}

/// Per-request rate breakdown — same labels as `speed_tip`, for the detail
/// table's row tooltip second line.
fn event_speed_tip(r: &EventRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((v, _)) = r.total_tps() {
        parts.push(tf!("总 {}", fmt::speed(v)));
    }
    if let Some((v, _)) = r.input_tps() {
        parts.push(tf!("输入 {}", fmt::speed(v)));
    }
    if let Some((v, _)) = r.output_tps() {
        parts.push(tf!("输出 {}", fmt::speed(v)));
    }
    if let Some(v) = r.prefill_tps() {
        parts.push(tf!("预填 {}", fmt::speed(v)));
    }
    if let Some((v, _)) = r.decode_tps() {
        parts.push(tf!("解码 {}", fmt::speed(v)));
    }
    if let Some((_, est)) = r.effective_ms()
        && est
    {
        parts.push(t!("时长按首尾时间戳估算").to_string());
    }
    parts.join(" · ")
}

pub fn detail_page(
    snap: Option<&Snapshot>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    table: &TableArgs,
) -> Vec<View> {
    let Some(s) = snap else {
        return vec![loading(theme, true)];
    };
    let d = &s.detail;
    // Virtualized: only rows scrolled into view are ever built or mounted
    // (~25 of a 200-row page) — the old eager list mounted ~4000 elements
    // in one UI-thread turn and re-arranged them every slide frame. Keys are
    // row indices, so a page flip refreshes realized rows in place.
    let empty = d.rows.is_empty();
    let list: View = if empty {
        // An empty page used to show nothing at all under the title.
        w::empty_state(theme, "这段时间没有事件")
    } else {
        virtual_rows(theme, d.rows.clone(), d.rows.len(), table.width, event_row)
    };
    let ruler = table_ruler(ctx, "detail-ruler", table.ruler);
    let pages = (d.total as i64 + DETAIL_PAGE_SIZE - 1) / DETAIL_PAGE_SIZE;
    let page = d.page;
    let mut nav: Vec<View> = Vec::new();
    if page > 0 {
        nav.push(
            Button::new()
                .on_click(ctx.callback(move |_| Msg::DetailPage(page - 1)))
                .content(t!("← 上一页")),
        );
    }
    nav.push(
        TextBlock::new()
            .text(tf!(
                "第 {} / {} 页 · 共 {} 条",
                page + 1,
                pages.max(1),
                d.total
            ))
            .font_size(theme.body_size)
            .foreground(theme.subtle)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    );
    if page + 1 < pages {
        nav.push(
            Button::new()
                .on_click(ctx.callback(move |_| Msg::DetailPage(page + 1)))
                .content(t!("下一页 →")),
        );
    }
    // Peers share hourly totals, not events: the table is this PC's alone.
    if s.vm.devices.len() > 1 {
        nav.push(
            TextBlock::new()
                .text(t!("仅本机事件"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }

    vec![
        header(
            theme,
            t!("明细"),
            vec![
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(8.0)
                    .keyed_children(keyed(nav)),
            ],
        ),
        w::card(
            theme,
            if empty {
                list
            } else {
                vstack(0.0, vec![ruler, detail_header(theme, table.width), list])
            },
        ),
    ]
}

/// What a virtualized table page needs from the shell: the measured row
/// width (0 = not measured yet) and the page's own width ruler.
pub struct TableArgs<'a> {
    pub width: f64,
    pub ruler: &'a ElementRef<SwapChainPanel>,
}

/// 1-DIP full-width ruler inside the table card: its surface Metrics report
/// the exact width a row must span (`Msg::SetTableWidth`, whole DIPs only, so
/// a resize drag rebuilds ~25 realized rows per step, not the page).
fn table_ruler(
    ctx: &mut ViewContext<Shell>,
    key: &'static str,
    ruler: &ElementRef<SwapChainPanel>,
) -> View {
    let on_width = ctx.callback(|w: f64| Msg::SetTableWidth(w));
    ctx.use_effect(key, (), {
        let ruler = ruler.clone();
        move || {
            let last = std::cell::Cell::new(0.0f64);
            let obs = ruler.observe_surface(move |event| {
                if let SwapChainPanelEvent::Metrics { width, .. } = event {
                    let w = width.floor();
                    if w > 0.0 && w != last.get() {
                        last.set(w);
                        let _ = on_width.call(w);
                    }
                }
            });
            Some(Box::new(move || drop(obs)))
        }
    });
    SwapChainPanel::new().element_ref(ruler).height(1.0).into()
}

/// Virtualized table body: `row(theme, &item, zebra)` runs only for indices
/// the ItemsRepeater realizes. `Arc` data + a cloned `Theme` keep the closure
/// `'static` without copying rows; keys are indices, so a same-length data
/// swap just re-reconciles the realized rows (no collection reset).
///
/// `width` is explicit because ItemsRepeater hosts every row in a
/// ContentControl whose HorizontalContentAlignment is Left: a row is laid out
/// at its *content* width, so the Grid's `*` column collapses and the numeric
/// columns no longer line up with the full-width header. (A huge `MinWidth`
/// does not help — XAML does not clamp it to the available width; the row
/// really becomes that wide and its right-hand columns fall off the card.)
fn virtual_rows<T: 'static>(
    theme: &Theme,
    rows: std::sync::Arc<Vec<T>>,
    shown: usize,
    width: f64,
    row: fn(&Theme, &T, bool, f64) -> View,
) -> View {
    let theme = theme.clone();
    ItemsRepeater::new()
        .virtual_source(VirtualSource::new(
            0,
            shown.min(rows.len()),
            |i| i as u64,
            move |i| {
                let body = row(&theme, &rows[i], i % 2 == 1, width);
                if width > 0.0 {
                    Border::new().width(width).content(body)
                } else {
                    body
                }
            },
        ))
        .into()
}

// ---------------------------------------------------------------- quota

/// One quota window row inside an app group card — distinct labels per
/// window_kind, percent badge + bar when known, usage/limit and reset on the
/// meta line.
fn quota_row(theme: &Theme, q: &globaltokentracker_core::store::QuotaRow) -> View {
    let label = tr(globaltokentracker_core::viewmodel::quota_kind_label(
        &q.window_kind,
    ));
    let mut title: Vec<View> = vec![
        TextBlock::new()
            .text(label)
            .font_size(theme.body_size)
            .font_weight(FontWeight::SEMI_BOLD)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ];
    if let Some(acc) = &q.account {
        title.push(w::badge(theme, acc.clone(), w::BadgeTone::Muted));
    }
    let credits = q.window_kind == "credits";
    let mut head: Vec<View> = vec![cell(
        0,
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .keyed_children(keyed(title)),
    )];
    // Right-hand pill: the balance for credit rows (they have no percent),
    // the used percent otherwise.
    if credits {
        if let Some(u) = q.used {
            head.push(cell(
                1,
                w::badge(
                    theme,
                    tf!("余额 {}", i18n::compact(u as u64)),
                    w::BadgeTone::Muted,
                )
                .tooltip(tf!("余额 {}", fmt::tokens_exact(u as u64))),
            ));
        }
    } else if let Some(pct) = q.used_percent {
        head.push(cell(
            1,
            w::badge(
                theme,
                format!("{}%", fmt::trim_f(pct, 1)),
                if pct > 80.0 {
                    w::BadgeTone::Danger
                } else if pct > 50.0 {
                    w::BadgeTone::Warn
                } else {
                    w::BadgeTone::Muted
                },
            ),
        ));
    }
    let mut body: Vec<View> = vec![
        Grid::new()
            .columns([GridLength::STAR, GridLength::Auto])
            .keyed_children(keyed(head)),
    ];
    if let Some(pct) = q.used_percent {
        body.push(
            ProgressBar::new()
                .value(pct)
                .maximum(100.0)
                .minimum(0.0)
                .into(),
        );
    }
    // `credits` rows carry remaining balance in `used` (see quota.rs) — it is
    // the pill above, never an "已用" figure. Compact 万/亿 keeps the meta line
    // scannable; exact counts go on the tooltip for reconciliation.
    let (usage, usage_exact) = if credits {
        (None, None)
    } else {
        let pair = |(u, l): (f64, f64)| {
            (
                tf!(
                    "已用 {} / 上限 {}",
                    i18n::compact(u as u64),
                    i18n::compact(l as u64)
                ),
                tf!(
                    "已用 {} / 上限 {}",
                    fmt::tokens_exact(u as u64),
                    fmt::tokens_exact(l as u64)
                ),
            )
        };
        match (q.used, q.limit_value) {
            (Some(u), Some(l)) => {
                let (c, e) = pair((u, l));
                (Some(c), Some(e))
            }
            (Some(u), None) => (
                Some(tf!("用量 {}", i18n::compact(u as u64))),
                Some(tf!("用量 {}", fmt::tokens_exact(u as u64))),
            ),
            (None, Some(l)) => (
                Some(tf!("上限 {}", i18n::compact(l as u64))),
                Some(tf!("上限 {}", fmt::tokens_exact(l as u64))),
            ),
            (None, None) => (None, None),
        }
    };
    // usage · reset · collected — a part a tool doesn't report is left out
    // instead of printing "重置 —".
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.extend(usage);
    let reset = reset_text(q.resets_at);
    if !reset.is_empty() {
        parts.push(reset);
    }
    parts.push(tf!("采集 {}", fmt::ts_short(Some(q.captured_at))));
    let mut meta_v: View = TextBlock::new()
        .text(parts.join(" · "))
        .text_wrapping(TextWrapping::Wrap)
        .font_size(theme.label_size)
        .foreground(theme.subtle)
        .into();
    if let Some(e) = usage_exact {
        meta_v = meta_v.tooltip(e);
    }
    body.push(meta_v);
    vstack(6.0, body)
}

/// Quota page — one collapsible card per tool (a vendor can expose several
/// windows; a flat list once flooded the page with WorkBuddy session marks).
pub fn quota_page(
    snap: Option<&Snapshot>,
    theme: &Theme,
    collapsed: &std::collections::BTreeSet<String>,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    let Some(s) = snap else {
        return vec![loading(theme, true)];
    };
    let mut list: Vec<View> = Vec::new();
    for g in &s.vm.quota_groups {
        let open = !collapsed.contains(&g.app);
        let worst = g.worst_pct.unwrap_or(0.0);
        let app_key = g.app.clone();
        // In-page toggle rather than WinUI's `Expander`: that control measures
        // its content at the content's own width and centres it, so the rows
        // collapsed into a 147 DIP column in the middle of the page (Stretch
        // and a Grid wrapper did not change it). Here the header is a button
        // that stretches, and the rows simply follow it.
        let header = Grid::new()
            .columns([GridLength::STAR, GridLength::Auto, GridLength::Auto])
            .column_spacing(8.0)
            .children([
                cell(
                    0,
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(8.0)
                        .children((
                            SymbolIcon::new().symbol(Symbol::Clock),
                            TextBlock::new()
                                .text(tf!("{} · {} 项配额", g.display, g.rows.len()))
                                .font_weight(FontWeight::SEMI_BOLD)
                                .vertical_alignment(VerticalAlignment::Center),
                        )),
                ),
                cell(
                    1,
                    Border::new()
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(w::badge(
                            theme,
                            format!("{}%", fmt::trim_f(worst, 1)),
                            if worst > 80.0 {
                                w::BadgeTone::Danger
                            } else if worst > 50.0 {
                                w::BadgeTone::Warn
                            } else {
                                w::BadgeTone::Muted
                            },
                        )),
                ),
                cell(
                    2,
                    Border::new()
                        .vertical_alignment(VerticalAlignment::Center)
                        .content(
                            TextBlock::new()
                                .text(if open { "▴" } else { "▾" })
                                .font_size(theme.label_size)
                                .foreground(theme.subtle)
                                .vertical_alignment(VerticalAlignment::Center),
                        ),
                ),
            ]);
        let header_btn = Button::new()
            .automation_name(tf!("配额组 {}", g.display))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .on_click(ctx.callback(move |_| Msg::SetQuotaGroup(app_key.clone(), !open)))
            .content(header);
        let mut inner: Vec<View> = vec![header_btn];
        if open {
            // A hairline between windows keeps rows with and without a bar
            // reading as separate entries.
            for (i, q) in g.rows.iter().enumerate() {
                if i > 0 && theme.line_separators {
                    inner.push(Border::new().height(1.0).background(theme.divider).into());
                }
                inner.push(quota_row(theme, q));
            }
        }
        list.push(w::card(theme, vstack(10.0, inner)));
    }
    // Quotas whose freshest report came from another device: one card, each
    // row naming the device. Never summed — the newest report per account.
    let others: Vec<&globaltokentracker_core::peer::merged::MergedQuota> =
        s.vm.quotas_all_devices
            .iter()
            .filter(|q| !q.local)
            .collect();
    if !others.is_empty() {
        let mut inner: Vec<View> =
            vec![w::section_header(theme, Symbol::Remote, t!("来自其他设备"))];
        for (i, q) in others.iter().enumerate() {
            if i > 0 && theme.line_separators {
                inner.push(Border::new().height(1.0).background(theme.divider).into());
            }
            let row = globaltokentracker_core::store::QuotaRow {
                app: q.app.clone(),
                account: None,
                captured_at: q.captured_at,
                window_kind: q.window_kind.clone(),
                used: q.used,
                limit_value: q.limit_value,
                used_percent: q.used_percent,
                resets_at: q.resets_at,
            };
            inner.push(vstack(
                4.0,
                vec![
                    TextBlock::new()
                        .text(tf!("{} · 来自 {}", app_display(&q.app), q.device_name))
                        .font_size(theme.label_size)
                        .foreground(theme.subtle)
                        .into(),
                    quota_row(theme, &row),
                ],
            ));
        }
        inner.push(
            TextBlock::new()
                .text(t!("每个账号取最新的一条；额度绝不跨设备相加。"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
        list.push(w::card(theme, vstack(10.0, inner)));
    }
    if list.is_empty() {
        list.push(w::card(theme, w::empty_state(theme, "暂无配额数据")));
    }
    // Cards animate individually — flatten header+cards so each is its own
    // spring block rather than one static list stack.
    std::iter::once(header(theme, t!("配额"), vec![]))
        .chain(list)
        .collect()
}

// ---------------------------------------------------------------- sources

pub fn sources_page(
    list_src: Option<&[globaltokentracker_core::store::SourceHealth]>,
    theme: &Theme,
) -> Vec<View> {
    let Some(list_src) = list_src else {
        return vec![loading(theme, true)];
    };
    let mut list: Vec<View> = Vec::new();
    for h in list_src {
        let err = h.last_error.is_some();
        // The status column carries a compact word, not the whole message: a
        // long error used to sit in an Auto column and squeeze the source name
        // to zero. The message gets its own wrapping line in the left column.
        let status = if err { t!("异常") } else { t!("正常") };
        let mut rows: Vec<View> = vec![
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .children((
                    SymbolIcon::new().symbol(Symbol::World),
                    TextBlock::new()
                        .text(app_display(&h.source).to_string())
                        .font_weight(FontWeight::SEMI_BOLD)
                        .vertical_alignment(VerticalAlignment::Center),
                )),
            TextBlock::new()
                .text(tf!(
                    "{} 文件 · 累计 {} 行 · 游标 {} · 上次 {}",
                    h.files_seen,
                    h.rows_ingested,
                    h.cursors,
                    fmt::ts_short(h.last_synced_at)
                ))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        ];
        if let Some(e) = &h.last_error {
            rows.push(
                TextBlock::new()
                    .text(tf!("⚠ {e}", e))
                    .font_size(theme.label_size)
                    .foreground(theme.danger)
                    .text_wrapping(TextWrapping::Wrap)
                    .into(),
            );
        }
        list.push(w::card(
            theme,
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(12.0)
                .children([
                    cell(
                        0,
                        StackPanel::new()
                            .orientation(Orientation::Vertical)
                            .spacing(4.0)
                            .keyed_children(keyed(rows)),
                    ),
                    cell(
                        1,
                        if err {
                            w::badge(theme, status.to_string(), w::BadgeTone::Danger)
                        } else {
                            TextBlock::new()
                                .text(status)
                                .font_size(theme.label_size)
                                .foreground(theme.ok)
                                .vertical_alignment(VerticalAlignment::Center)
                                .into()
                        },
                    ),
                ]),
        ));
    }
    if list.is_empty() {
        list.push(w::card(theme, w::empty_state(theme, "尚未扫描")));
    }
    std::iter::once(header(theme, t!("数据源"), vec![]))
        .chain(list)
        .collect()
}

// ---------------------------------------------------------------- devices

/// Devices page inputs (shell state the page renders).
pub struct DevicesArgs<'a> {
    pub info: Option<&'a crate::devices::DevicesInfo>,
    pub nearby: &'a [globaltokentracker_core::peer::discovery::Found],
    pub syncing: bool,
    pub discovering: bool,
    pub error: Option<&'a str>,
    pub pair: &'a crate::devices::PairForm,
}

fn subtle_text(theme: &Theme, text: String) -> View {
    TextBlock::new()
        .text(text)
        .font_size(theme.label_size)
        .foreground(theme.subtle)
        .text_wrapping(TextWrapping::Wrap)
        .into()
}

fn warn_text(theme: &Theme, text: String) -> View {
    TextBlock::new()
        .text(text)
        .font_size(theme.label_size)
        .foreground(theme.warn)
        .text_wrapping(TextWrapping::Wrap)
        .into()
}

/// One paired device: name + address, sync status (or the last error in
/// words), its span totals, the include switch and Forget.
fn paired_device_card(
    theme: &Theme,
    p: &globaltokentracker_core::peer::devices::PeerDevice,
    span: Option<&globaltokentracker_core::store::Totals>,
    range_label: &str,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let mut lines: Vec<View> = vec![
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((
                SymbolIcon::new().symbol(Symbol::Remote),
                TextBlock::new()
                    .text(p.name.clone())
                    .font_weight(FontWeight::SEMI_BOLD)
                    .vertical_alignment(VerticalAlignment::Center),
                w::badge(theme, p.last_address.clone(), w::BadgeTone::Muted),
            )),
    ];
    match &p.last_error_code {
        Some(code) => lines.push(warn_text(theme, crate::devices::message(code))),
        None => lines.push(subtle_text(
            theme,
            match p.last_seen_at {
                Some(t) => tf!("同步于 {}", fmt::ts_short(Some(t))),
                None => t!("尚未同步").to_string(),
            },
        )),
    }
    if let (Some(t), true) = (span, p.enabled) {
        lines.push(subtle_text(
            theme,
            tf!(
                "{} · {} tok · {}",
                range_label,
                fmt::tokens_exact(fmt::tokens_total(t)),
                fmt::usd(t.cost_usd)
            ),
        ));
    }
    if let Some(v) = &p.gtt_version {
        lines.push(subtle_text(theme, tf!("版本 {}", v)));
    }
    let id = p.device_id.clone();
    let (fid, fname) = (p.device_id.clone(), p.name.clone());
    w::card(
        theme,
        Grid::new()
            .columns([GridLength::STAR, GridLength::Auto, GridLength::Auto])
            .column_spacing(12.0)
            .children([
                cell(0, vstack(4.0, lines)),
                cell(
                    1,
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(6.0)
                        .vertical_alignment(VerticalAlignment::Center)
                        .children((
                            TextBlock::new()
                                .text(t!("计入"))
                                .font_size(theme.label_size)
                                .foreground(theme.subtle)
                                .vertical_alignment(VerticalAlignment::Center),
                            switch(
                                theme,
                                p.enabled,
                                ctx.callback(move |on: bool| Msg::SetPeerEnabled(id.clone(), on)),
                            ),
                        )),
                ),
                cell(
                    2,
                    Button::new()
                        .vertical_alignment(VerticalAlignment::Center)
                        .on_click(ctx.callback(move |_| Msg::ForgetAsk(fid.clone(), fname.clone())))
                        .content(t!("忘记…")),
                ),
            ]),
    )
}

/// LAN multi-device viewing: this PC, paired devices, possible double
/// counting, the Add-device form and devices found nearby.
pub fn devices_page(
    snap: Option<&Snapshot>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    args: &DevicesArgs,
) -> Vec<View> {
    let Some(info) = args.info else {
        return vec![header(theme, t!("设备"), vec![]), loading(theme, true)];
    };
    let has_peers = !info.peers.is_empty();
    let sync_btn: View = Button::new()
        .is_enabled(has_peers && !args.syncing)
        .on_click(ctx.callback(|_| Msg::SyncPeers { if_due: false }))
        .content(if args.syncing {
            t!("正在同步…")
        } else {
            t!("立即同步")
        });
    let mut blocks: Vec<View> = vec![header(theme, t!("设备"), vec![sync_btn])];

    // This PC.
    blocks.push(w::section(
        theme,
        Symbol::Home,
        t!("本机"),
        vec![w::card(
            theme,
            vstack(
                6.0,
                vec![
                    TextBlock::new()
                        .text(crate::devices::local_label(&info.local_name))
                        .font_weight(FontWeight::SEMI_BOLD)
                        .into(),
                    subtle_text(
                        theme,
                        tf!(
                            "设备 ID {}",
                            info.local_id.chars().take(8).collect::<String>()
                        ),
                    ),
                    subtle_text(
                        theme,
                        t!("在“设置 → 局域网共享”中开启后，其他设备即可与本机配对并查看本机用量。")
                            .into(),
                    ),
                ],
            ),
        )],
    ));

    // Paired devices.
    let range_label = snap.map_or_else(String::new, |s| tr(s.vm.range.label()).to_string());
    let mut paired: Vec<View> = Vec::new();
    if has_peers {
        for p in &info.peers {
            let span = snap.and_then(|s| {
                s.vm.devices
                    .iter()
                    .find(|d| d.device_id == p.device_id)
                    .map(|d| &d.span)
            });
            paired.push(paired_device_card(theme, p, span, &range_label, ctx));
        }
    } else {
        paired.push(w::card(
            theme,
            subtle_text(
                theme,
                t!("尚未配对任何设备。在另一台设备上开始共享以获得配对码，然后在下方添加设备。")
                    .into(),
            ),
        ));
    }
    if let Some(e) = args.error {
        // Inside a card: a bare paragraph between two sections read as part of
        // whichever section happened to be above it.
        paired.push(w::card(theme, warn_text(theme, crate::devices::message(e))));
    }
    blocks.push(w::section(theme, Symbol::Link, t!("已配对的设备"), paired));

    // Possible double counting.
    if !info.warnings.is_empty() {
        let mut rows: Vec<View> = Vec::new();
        for wr in &info.warnings {
            rows.push(warn_text(
                theme,
                tf!(
                    "{} 与 {} 有 {} 天的 {} 用量完全相同，可能扫描了同一个同步文件夹；合计中两者仍都计入。",
                    info.name_of(&wr.device_a),
                    info.name_of(&wr.device_b),
                    wr.days,
                    app_display(&wr.app)
                ),
            ));
        }
        blocks.push(w::section(
            theme,
            Symbol::Important,
            t!("可能重复计算"),
            vec![w::card(theme, vstack(8.0, rows))],
        ));
    }

    // Add device.
    let pair = args.pair;
    let mut form: Vec<View> = vec![
        subtle_text(
            theme,
            t!("在另一台设备上开始共享，它会显示一个 6 位配对码，2 分钟内有效且只能使用一次。配对是双向的：完成后两台设备都能看到彼此的用量，本机也会开始共享。")
                .into(),
        ),
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(10.0)
            .children((
                TextBox::new()
                    .text(pair.addr.clone())
                    .placeholder_text("192.168.1.20:4319")
                    .on_text_changed(ctx.callback(Msg::PairAddr))
                    .width(240.0),
                TextBox::new()
                    .text(pair.code.clone())
                    .placeholder_text(t!("配对码"))
                    .on_text_changed(ctx.callback(Msg::PairCode))
                    .width(120.0),
                Button::new()
                    .is_enabled(pair.ready())
                    .on_click(ctx.callback(|_| Msg::PairSubmit))
                    .content(if pair.busy {
                        t!("正在配对…")
                    } else {
                        t!("配对")
                    }),
            )),
    ];
    if let Some(code) = &pair.error {
        form.push(warn_text(theme, crate::devices::message(code)));
    } else if let Some(name) = &pair.paired {
        form.push(
            TextBlock::new()
                .text(tf!("已与 {} 配对", name))
                .font_size(theme.label_size)
                .foreground(theme.ok)
                .into(),
        );
    }
    blocks.push(w::section(
        theme,
        Symbol::Add,
        t!("添加设备"),
        vec![w::card(theme, vstack(10.0, form))],
    ));

    // Nearby (Bonjour).
    let find_btn: View = Button::new()
        .is_enabled(!args.discovering)
        .on_click(ctx.callback(|_| Msg::Discover))
        .content(if args.discovering {
            t!("正在查找设备…")
        } else {
            t!("查找设备")
        });
    let mut nearby: Vec<View> = Vec::new();
    for d in args.nearby {
        let mut left: Vec<View> = vec![
            TextBlock::new()
                .text(d.name.clone())
                .font_weight(FontWeight::SEMI_BOLD)
                .into(),
            subtle_text(theme, d.addresses.first().cloned().unwrap_or_default()),
        ];
        if d.warnings.iter().any(|w| w == "fingerprint_mismatch") {
            left.push(warn_text(
                theme,
                t!("其身份与该 ID 的已配对设备不一致——拉取将被拒绝。").into(),
            ));
        } else if d.warnings.iter().any(|w| w == "duplicate_device_id") {
            left.push(warn_text(
                theme,
                t!("另一台设备使用了相同的 ID（账本被复制过？）。").into(),
            ));
        }
        let right: View = if d.paired {
            w::badge(theme, t!("已配对").into(), w::BadgeTone::Accent)
        } else {
            let addr = d.addresses.first().cloned().unwrap_or_default();
            Button::new()
                .is_enabled(!addr.is_empty())
                .on_click(ctx.callback(move |_| Msg::PairUse(addr.clone())))
                .content(t!("用此地址配对"))
        };
        nearby.push(
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .children([cell(0, vstack(2.0, left)), cell(1, right)]),
        );
    }
    if nearby.is_empty() {
        nearby.push(subtle_text(
            theme,
            t!("选择“查找设备”以搜索此网络中正在共享的 GlobalTokenTracker 设备。").into(),
        ));
    }
    nearby.push(subtle_text(
        theme,
        t!("没有列出？访客 Wi-Fi、VLAN 和客户端隔离会阻止发现——请改用地址添加设备，例如 192.168.1.20:4319。").into(),
    ));
    nearby.push(card_rule(theme));
    nearby.push(subtle_text(
        theme,
        t!("设备之间只共享按小时汇总的用量，从不共享提示词、会话、项目或路径。额度按账号计算，绝不跨设备相加。").into(),
    ));
    blocks.push(w::section_with(
        theme,
        Symbol::Globe,
        t!("附近"),
        vec![find_btn],
        vec![w::card(theme, vstack(10.0, nearby))],
    ));
    blocks
}

// ---------------------------------------------------------------- prices

// -------------------------------------------------------------- prices
// Logical columns: model | input | output | cache-read | cache-write | source.
// Narrower windows drop cache-write (mostly 0), then cache-read.
const PRICE_PLANS: [Plan; 4] = [
    &[
        (0, None),
        (1, Some(96.0)),
        (2, Some(96.0)),
        (3, Some(96.0)),
        (4, Some(96.0)),
        (5, Some(110.0)),
    ],
    &[
        (0, None),
        (1, Some(76.0)),
        (2, Some(76.0)),
        (3, Some(76.0)),
        (4, Some(76.0)),
        (5, Some(96.0)),
    ],
    &[
        (0, None),
        (1, Some(76.0)),
        (2, Some(76.0)),
        (3, Some(76.0)),
        (5, Some(96.0)),
    ],
    &[(0, None), (1, Some(76.0)), (2, Some(76.0)), (5, Some(96.0))],
];

/// Wide-window plan for high-resolution displays (width >= ~1350 DIP):
/// offers generous cell widths for $/1M token prices and corroboration badges.
pub const PRICE_PLAN_WIDE: Plan = &[
    (0, None),
    (1, Some(112.0)),
    (2, Some(112.0)),
    (3, Some(112.0)),
    (4, Some(112.0)),
    (5, Some(136.0)),
];

fn price_layout(theme: &Theme, width: f64) -> (Vec<GridLength>, [Option<i32>; 6]) {
    let k = font_scale(theme);
    let plan = if width >= 1350.0 * k {
        PRICE_PLAN_WIDE
    } else {
        pick_plan(&PRICE_PLANS, width, k)
    };
    plan_layout(plan, k)
}

/// $/1M values vary from 0.0001 to thousands — trim, don't pad.
fn price_num(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".into()
    } else {
        s.to_string()
    }
}

fn price_head(theme: &Theme, width: f64) -> View {
    let (cols, at) = price_layout(theme, width);
    let head_cell = |logical: usize, h: &str| -> Option<View> {
        at[logical].map(|i| {
            let cell = dcell(
                i,
                dtext(theme, h.into(), logical > 0)
                    .font_size(theme.label_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(theme.subtle)
                    .into(),
            );
            if logical == 5 {
                cell.tooltip(t!(
                    "与所示价格一致的来源数 / 给出报价的来源总数。橙色 = 来源之间有分歧；悬停行查看各家报价。"
                ))
            } else {
                cell
            }
        })
    };
    let cells: Vec<View> = [
        head_cell(0, t!("模型")),
        head_cell(1, t!("输入")),
        head_cell(2, t!("输出")),
        head_cell(3, t!("缓存读")),
        head_cell(4, t!("缓存写")),
        head_cell(5, t!("佐证")),
    ]
    .into_iter()
    .flatten()
    .collect();
    Border::new()
        .padding(Thickness::xy(10.0, 6.0))
        .border_brush(theme.divider)
        .border_thickness(Thickness::new(0.0, 0.0, 0.0, 1.0))
        .content(Grid::new().columns(cols).keyed_children(keyed(cells)))
}

/// The "佐证" cell: how many sources back the shown price, out of those that
/// quoted one. Orange = at least one source says something else.
fn corroboration(theme: &Theme, p: &PriceRow) -> View {
    let tone = if p.total == 0 || p.source == "seed" || p.total == 1 {
        w::BadgeTone::Muted
    } else if p.disputed() {
        w::BadgeTone::Warn
    } else {
        w::BadgeTone::Accent
    };
    let text = if p.total == 0 {
        "—".to_string()
    } else {
        format!("{}/{}", p.agree, p.total)
    };
    Border::new()
        .horizontal_alignment(HorizontalAlignment::Right)
        .content(w::badge(theme, text, tone))
}

/// Hover text of a price row: the chosen price and every source's quote
/// (✓ backs it, ✗ says otherwise, – has no price, · is not counted).
fn price_tooltip(p: &PriceRow) -> String {
    let money = |i: f64, o: f64| format!("{} / {}", price_num(i), price_num(o));
    let mut lines = vec![p.model.clone()];
    let mut head = money(p.input, p.output);
    if p.cache_read > 0.0 {
        head.push_str(&tf!(" · 缓存读 {}", price_num(p.cache_read)));
    }
    if p.cache_write > 0.0 {
        head.push_str(&tf!(" · 缓存写 {}", price_num(p.cache_write)));
    }
    lines.push(head);
    if p.total > 0 {
        lines.push(tf!("采信 {}（{}/{} 家一致）", p.source, p.agree, p.total));
    }
    lines.push(String::new());
    for q in &p.quotes {
        let (mark, detail) = match q.stance {
            Stance::Agrees => ("✓", money(q.input, q.output)),
            Stance::Dissents => ("✗", money(q.input, q.output)),
            Stance::NoData => ("–", t!("无报价").to_string()),
            Stance::Ignored => ("·", tf!("{}（未计票）", money(q.input, q.output))),
        };
        let alias = if q.key == p.model {
            String::new()
        } else {
            format!("  [{}]", q.key)
        };
        lines.push(format!("{mark} {:<11} {detail}{alias}", q.source));
    }
    lines.join("\n")
}

fn price_row(theme: &Theme, p: &PriceRow, zebra: bool, width: f64) -> View {
    let (cols, at) = price_layout(theme, width);
    let put = |logical: usize, v: View| at[logical].map(|i| dcell(i, v));
    let cells: Vec<View> = [
        put(0, dtext(theme, p.model.clone(), false).into()),
        put(1, dtext(theme, price_num(p.input), true).into()),
        put(2, dtext(theme, price_num(p.output), true).into()),
        put(
            3,
            dtext(theme, price_num(p.cache_read), true)
                .foreground(theme.subtle)
                .into(),
        ),
        put(
            4,
            dtext(theme, price_num(p.cache_write), true)
                .foreground(theme.subtle)
                .into(),
        ),
        put(5, corroboration(theme, p)),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Same chrome as detail rows: separators gated by theme, alternating
    // ~4% zebra, the evidence on hover.
    let mut row = Border::new().padding(Thickness::xy(10.0, 5.0));
    if theme.line_separators {
        row = row
            .border_brush(theme.divider)
            .border_thickness(Thickness::new(0.0, 0.0, 0.0, 1.0));
    }
    if zebra {
        row = row.background(Brush::Solid(Color::argb(10, 128, 128, 128)));
    }
    row.content(Grid::new().columns(cols).keyed_children(keyed(cells)))
        .tooltip(price_tooltip(p))
}

/// The prices page's search state (owned by the shell).
pub struct PriceSearch<'a> {
    pub query: &'a str,
    pub disputed: bool,
    /// The rows that match — the whole book when nothing is filtered.
    pub shown: &'a std::sync::Arc<Vec<PriceRow>>,
}

/// State of the manual "刷新价目" action: a refresh in flight, and why the last
/// one produced nothing.
pub struct PriceRefresh<'a> {
    pub active: bool,
    pub error: Option<&'a str>,
}

pub fn prices_page(
    table: Option<&PriceTable>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    args: &TableArgs,
    search: &PriceSearch,
    refresh: &PriceRefresh,
) -> Vec<View> {
    let Some(table) = table else {
        return vec![loading(theme, true)];
    };
    let total = table.rows.len();
    let shown = search.shown.len();
    let filtering = !search.query.trim().is_empty() || search.disputed;
    // Virtualized like the detail table: only the rows scrolled into view are
    // ever mounted, so the whole book (thousands of models) is browsable.
    let list: View = virtual_rows(theme, search.shown.clone(), shown, args.width, price_row);
    let ruler = table_ruler(ctx, "prices-ruler", args.ruler);

    // Search box (fed back its own text — see `Shell::price_query`) + the
    // dispute filter.
    // Stretches with the row: a fixed 380 DIP box overflowed a narrow window
    // (the checkbox beside it is Auto-sized).
    let search_box: View = TextBox::new()
        .text(search.query.to_string())
        .placeholder_text(t!("搜索模型，如 opus 4.6 或 gpt mini"))
        .on_text_changed(ctx.callback(Msg::PriceQuery))
        .into();
    let only_disputed: View = CheckBox::new()
        .is_checked(search.disputed)
        .on_is_checked_changed(ctx.callback(Msg::PriceDisputed))
        .content(
            TextBlock::new()
                .text(t!("仅看有分歧的"))
                .font_size(theme.body_size),
        );
    let sync = match table.synced_at {
        Some(t) => tf!(
            "联网同步于 {} 小时前",
            (globaltokentracker_core::store::now_ms() - t) / 3_600_000
        ),
        None => t!("仅本地种子，尚未联网同步").to_string(),
    };
    let summary = if filtering {
        tf!("匹配 {} / {} 个模型 · {}", shown, total, sync)
    } else {
        tf!("{} 个模型 · {}", total, sync)
    };
    let mut actions: Vec<View> = Vec::new();
    if refresh.active {
        actions.push(
            ProgressRing::new()
                .is_indeterminate(true)
                .is_active(true)
                .width(18.0)
                .height(18.0)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        actions.push(
            TextBlock::new()
                .text(t!("同步中…"))
                .font_size(theme.body_size)
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    actions.push(
        Button::new()
            .is_enabled(!refresh.active)
            .on_click(ctx.callback(|_| Msg::RefreshPrices))
            .content(t!("刷新价目")),
    );
    // Errors get their own wrapping line: appended to the one-line summary a
    // long feed error was simply cut off at the card edge.
    // One filter band under the title: the filters (search + dispute toggle)
    // as one group on the left, the result summary right-aligned on the same
    // centerline. The box used to stretch across the page with the checkbox
    // pinned under 刷新价目, so the two halves read as unrelated blocks.
    let summary: View = TextBlock::new()
        .text(summary)
        .font_size(theme.label_size)
        .foreground(theme.subtle)
        .text_wrapping(TextWrapping::Wrap)
        .horizontal_alignment(HorizontalAlignment::Right)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let controls: View = Grid::new()
        .columns([GridLength::Auto, GridLength::Auto, GridLength::STAR])
        .column_spacing(16.0)
        .children((
            Border::new()
                .vertical_alignment(VerticalAlignment::Center)
                // Wide enough for the placeholder, small enough to leave the
                // summary its line on a 1000 DIP window.
                .width(360.0)
                .content(search_box),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(only_disputed),
            Border::new()
                .grid_column(2)
                .vertical_alignment(VerticalAlignment::Center)
                .content(summary),
        ));
    // Errors keep their own wrapping line: appended to the summary a long
    // feed error was simply cut off.
    let controls = match refresh.error {
        Some(e) => vstack(
            6.0,
            vec![
                controls,
                TextBlock::new()
                    .text(tf!("上次刷新失败：{}", e))
                    .font_size(theme.label_size)
                    .foreground(theme.danger)
                    .text_wrapping(TextWrapping::Wrap)
                    .into(),
            ],
        ),
        None => controls,
    };

    let body: Vec<View> = if shown == 0 {
        vec![
            ruler,
            price_head(theme, args.width),
            TextBlock::new()
                .text(t!("没有匹配的模型"))
                .font_size(theme.body_size)
                .foreground(theme.subtle)
                .margin(Thickness::xy(10.0, 16.0))
                .into(),
        ]
    } else {
        vec![ruler, price_head(theme, args.width), list]
    };
    vec![
        header(theme, t!("价目表（$/1M tokens）"), actions),
        controls,
        // Card chrome around the table — same as the detail page.
        w::card(theme, vstack(0.0, body)),
    ]
}

// ---------------------------------------------------------------- settings

/// (semantic value, zh label) — labels go through `tr` at render so the
/// emitted text always matches the active locale.
const THEME_OPTIONS: &[(&str, &str)] =
    &[("system", "跟随系统"), ("light", "浅色"), ("dark", "深色")];
const ACCENT_OPTIONS: &[(&str, &str)] = &[
    ("", "默认"),
    ("#4d8ae8", "蓝色"),
    ("#10a874", "绿色"),
    ("#8b6fd8", "紫色"),
    ("#e8853d", "橙色"),
    ("#d84d5b", "红色"),
];
const LANG_OPTIONS: &[(&str, &str)] = &[("zh", "中文"), ("en", "English")];
const CURRENCY_OPTIONS: &[(&str, &str)] = &[
    ("USD", "USD $"),
    ("CNY", "CNY ¥"),
    ("EUR", "EUR €"),
    ("JPY", "JPY ¥"),
    ("GBP", "GBP £"),
    ("KRW", "KRW ₩"),
    ("HKD", "HKD HK$"),
    ("TWD", "TWD NT$"),
    ("CAD", "CAD CA$"),
    ("AUD", "AUD A$"),
    ("SGD", "SGD S$"),
    ("INR", "INR ₹"),
];
const CLOSE_OPTIONS: &[(&str, &str)] = &[
    ("", "每次询问"),
    ("quit", "彻底退出"),
    ("tray", "隐藏到托盘"),
];

/// Dropdown picker for one setting row. ComboBox reports the selected
/// index — semantic value lookup is by position, no label matching.
fn setting_dropdown(
    options: &'static [(&'static str, &'static str)],
    current: &str,
    ctx: &mut ViewContext<Shell>,
    map: impl Fn(&'static str) -> Msg + 'static + Clone,
) -> View {
    let selected = options.iter().position(|(value, _)| *value == current);
    ComboBox::new()
        .items_source(options.iter().map(|(_, label)| tr(label)))
        .selected_index(selected)
        .min_width(220.0)
        .on_selection_changed(ctx.callback(move |idx: Option<usize>| {
            map(idx
                .and_then(|i| options.get(i))
                .map(|(value, _)| *value)
                .unwrap_or_default())
        }))
        .into()
}

/// Label column + control + optional subtle note — consistent with the
/// pages' two-column chrome.
fn setting_row(
    theme: &Theme,
    label: &'static str,
    note: Option<&'static str>,
    control: View,
) -> View {
    let mut left: Vec<View> = vec![
        TextBlock::new()
            .text(tr(label))
            .font_size(theme.body_size)
            .font_weight(FontWeight::SEMI_BOLD)
            .text_wrapping(TextWrapping::Wrap)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ];
    if let Some(n) = note {
        // Wraps inside the label column — a long English note used to run
        // under the control column and get cut off.
        left.push(
            TextBlock::new()
                .text(tr(n))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    Grid::new()
        .columns([
            GridLength::Pixel(300.0 * font_scale(theme)),
            GridLength::STAR,
        ])
        // Same gutter as `setting_row_wide` (which had 16 while this one had
        // none) and the same label stack rhythm — two row builders in one card
        // put their controls on two different left edges.
        .column_spacing(16.0)
        .children([
            cell(
                0,
                StackPanel::new()
                    .spacing(3.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .keyed_children(keyed(left)),
            ),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .horizontal_alignment(HorizontalAlignment::Left)
                        .children([control]),
                ),
        ])
}

/// Font picker listing every DirectWrite family installed on the machine.
/// Index 0 is the default entry; `i` maps to `families()[i-1]`. Names are
/// shown as-is — they're already localized family names, not zh UI strings.
fn font_dropdown(current: &str, ctx: &mut ViewContext<Shell>) -> View {
    let families = crate::fonts::families();
    let selected = if current.is_empty() {
        Some(0)
    } else {
        families.iter().position(|f| f == current).map(|i| i + 1)
    };
    let labels: Vec<String> = std::iter::once(tr("默认（Segoe UI）").to_string())
        .chain(families.iter().cloned())
        .collect();
    ComboBox::new()
        .items_source(labels)
        .selected_index(selected)
        .min_width(220.0)
        .on_selection_changed(ctx.callback(|idx: Option<usize>| {
            Msg::SetFontFamily(
                idx.and_then(|i| crate::fonts::families().get(i.wrapping_sub(1)))
                    .filter(|_| idx.is_some_and(|i| i > 0))
                    .cloned()
                    .unwrap_or_default(),
            )
        }))
        .into()
}

/// Body-size slider — title/h2/label keep their offsets (see `SetFontSize`),
/// so one drag rescales the whole interface. Live-previews as it moves.
fn size_control(theme: &Theme, ctx: &mut ViewContext<Shell>) -> View {
    let slider: View = Slider::new()
        .minimum(9.0)
        .maximum(18.0)
        .step_frequency(0.5)
        .value(theme.body_size.clamp(9.0, 18.0))
        .width(220.0)
        .vertical_alignment(VerticalAlignment::Center)
        .on_value_changed(ctx.callback(Msg::SetFontSize))
        .into();
    let readout: View = TextBlock::new()
        .text(format!("{:.1}", theme.body_size))
        .font_size(theme.body_size)
        .min_width(30.0)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(12.0)
        .children([slider, readout])
}

/// Toggle whose On/Off caption follows the UI language — and sits *outside*
/// the control. A ToggleSwitch with OnContent/OffContent slots measures
/// ~150 DIP while it only draws ~64 (the hidden slot still counts toward the
/// desired size), which overflowed its column and pinned the switch to the
/// left edge instead of the right on every row.
pub(crate) fn switch(theme: &Theme, is_on: bool, toggled: Callback<bool>) -> View {
    // Caption suppressed and the track slot a Pixel column: the control's
    // 开/关 slot measures ~154 DIP while it draws ~44 — WinUI `width()` pins
    // the drawn track but not the desired size reported to the parent — so a
    // StackPanel kept ~110 DIP of phantom space between track and caption and
    // overflowed every fixed switch column (the caption landed past the card
    // edge, the track pinned to the column's left). Pixel columns are honored
    // in both measure and arrange, so the caption sits right after the track.
    Grid::new()
        .columns([GridLength::Pixel(44.0), GridLength::Auto])
        .column_spacing(8.0)
        .children((
            ToggleSwitch::new()
                .grid_column(0)
                .is_on(is_on)
                .on_toggled(toggled)
                .width(44.0)
                .vertical_alignment(VerticalAlignment::Center)
                .slots([
                    SlotView::new(
                        ToggleSwitchSlot::OnContent,
                        TextBlock::new().text("").font_size(theme.body_size),
                    ),
                    SlotView::new(
                        ToggleSwitchSlot::OffContent,
                        TextBlock::new().text("").font_size(theme.body_size),
                    ),
                ]),
            TextBlock::new()
                .grid_column(1)
                .text(if is_on { t!("开") } else { t!("关") })
                .font_size(theme.body_size)
                .vertical_alignment(VerticalAlignment::Center),
        ))
}

/// A settings row whose value column gets the full width (setting_row
/// keeps controls at their natural width in a horizontal stack, which left
/// wrapping text and right-aligned buttons no room).
fn setting_row_wide(
    theme: &Theme,
    label: &'static str,
    note: Option<&'static str>,
    value: View,
) -> View {
    let mut left: Vec<View> = vec![
        TextBlock::new()
            .text(tr(label))
            .font_size(theme.body_size)
            .font_weight(FontWeight::SEMI_BOLD)
            .text_wrapping(TextWrapping::Wrap)
            .into(),
    ];
    if let Some(n) = note {
        left.push(subtle_text(theme, tr(n).to_string()));
    }
    Grid::new()
        .columns([
            GridLength::Pixel(300.0 * font_scale(theme)),
            GridLength::STAR,
        ])
        .column_spacing(16.0)
        .children([
            cell(
                0,
                // Centered like `setting_row`: the two builders sit in the same
                // cards, and Top here made one card's labels jump against the
                // next one's.
                StackPanel::new()
                    .spacing(3.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .keyed_children(keyed(left)),
            ),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(value),
        ])
}

/// Hairline between groups inside a settings card.
fn card_rule(theme: &Theme) -> View {
    Border::new().height(1.0).background(theme.divider).into()
}

/// Settings "LAN sharing": the switch (with its live status beside it),
/// this PC's addresses and certificate, the one-time pairing code, the
/// viewers paired with this PC, and the firewall note. Every line sits in
/// the label | value columns or spans the card — no loose full-width text.
fn share_rows(
    config: &UiConfig,
    share: &crate::devices::ShareState,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    // The switch, and under it what sharing is doing right now.
    let mut switch_col: Vec<View> = vec![switch(
        theme,
        config.share_lan,
        ctx.callback(Msg::SetShareLan),
    )];
    if share.busy {
        switch_col.push(subtle_text(theme, t!("正在启动或停止共享…").into()));
    } else if let Some(e) = &share.error {
        switch_col.push(warn_text(theme, tf!("共享失败：{}", e)));
    }
    let mut rows: Vec<View> = vec![setting_row_wide(
        theme,
        "共享本机用量",
        Some(
            "同一局域网中已配对的设备可只读查看本机按小时汇总的用量。配对是双向的，任一端完成配对即可互相查看。",
        ),
        vstack(4.0, switch_col),
    )];

    if let Some(st) = &share.status {
        rows.push(card_rule(theme));
        // One address per line (several network adapters made one long,
        // clipped line), then the certificate the other side pins.
        let mut addr_col: Vec<View> = if st.addresses.is_empty() {
            vec![
                TextBlock::new()
                    .text(tf!("本机 IP:{}", st.port))
                    .font_size(theme.body_size)
                    .into(),
            ]
        } else {
            st.addresses
                .iter()
                .map(|a| {
                    TextBlock::new()
                        .text(a.clone())
                        .font_size(theme.body_size)
                        .is_text_selection_enabled(true)
                        .into()
                })
                .collect()
        };
        addr_col.push(subtle_text(
            theme,
            tf!(
                "证书指纹 {}…（对方配对后固定此身份）",
                st.fingerprint.chars().take(16).collect::<String>()
            ),
        ));
        rows.push(setting_row_wide(
            theme,
            "本机地址",
            Some("在另一台设备的“设备 → 添加设备”中填写此地址"),
            vstack(3.0, addr_col),
        ));

        // The code large on its own line with its button; when it expires
        // underneath.
        let code_view: View = match &st.code {
            Some(code) => {
                let until = share
                    .code_until
                    .map(|t| {
                        let ms = t
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() as i64);
                        tf!("有效至 {}", fmt::ts_short(Some(ms)))
                    })
                    .unwrap_or_default();
                vstack(
                    4.0,
                    vec![
                        StackPanel::new()
                            .orientation(Orientation::Horizontal)
                            .spacing(16.0)
                            .children((
                                TextBlock::new()
                                    .text(format!("{} {}", &code[..3], &code[3..]))
                                    .font_size(theme.title_size)
                                    .font_weight(FontWeight::SEMI_BOLD)
                                    .is_text_selection_enabled(true)
                                    .vertical_alignment(VerticalAlignment::Center),
                                Button::new()
                                    .vertical_alignment(VerticalAlignment::Center)
                                    .on_click(ctx.callback(|_| Msg::ShareNewCode))
                                    .content(t!("新配对码")),
                            )),
                        subtle_text(theme, until),
                    ],
                )
            }
            None => Button::new()
                .on_click(ctx.callback(|_| Msg::ShareNewCode))
                .content(t!("生成配对码")),
        };
        rows.push(setting_row_wide(
            theme,
            "配对码",
            Some("2 分钟内有效，只能使用一次；连续输错 5 次后作废"),
            code_view,
        ));
    }

    let active: Vec<&globaltokentracker_core::peer::trust::Viewer> = share
        .viewers
        .iter()
        .filter(|v| v.revoked_at.is_none())
        .collect();
    if !active.is_empty() {
        rows.push(card_rule(theme));
        // Spans the card: name and when on the left, Revoke at the right
        // edge, a hairline between viewers.
        let mut list: Vec<View> = vec![vstack(
            3.0,
            vec![
                TextBlock::new()
                    .text(tr("已配对的查看方"))
                    .font_size(theme.body_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .into(),
                subtle_text(theme, t!("撤销后立即失效，需重新配对才能再次查看").into()),
            ],
        )];
        for (i, v) in active.into_iter().enumerate() {
            let id = v.id.clone();
            if i > 0 {
                list.push(card_rule(theme));
            }
            list.push(
                Border::new().padding(Thickness::xy(0.0, 4.0)).content(
                    Grid::new()
                        .columns([GridLength::STAR, GridLength::Auto])
                        .column_spacing(16.0)
                        .children([
                            cell(
                                0,
                                vstack(
                                    2.0,
                                    vec![
                                        TextBlock::new()
                                            .text(v.name.clone())
                                            .font_size(theme.body_size)
                                            .text_trimming(TextTrimming::CharacterEllipsis)
                                            .into(),
                                        subtle_text(
                                            theme,
                                            tf!(
                                                "配对于 {} · 最近访问 {}",
                                                fmt::ts_short(Some(v.created_at)),
                                                fmt::ts_short(v.last_seen_at)
                                            ),
                                        ),
                                    ],
                                ),
                            ),
                            Border::new()
                                .grid_column(1)
                                .vertical_alignment(VerticalAlignment::Center)
                                .content(
                                    Button::new()
                                        .on_click(ctx.callback(move |_| Msg::Revoke(id.clone())))
                                        .content(t!("撤销")),
                                ),
                        ]),
                ),
            );
        }
        rows.push(vstack(6.0, list));
    }

    // Footnote, set apart from the controls.
    rows.push(card_rule(theme));
    rows.push(subtle_text(
        theme,
        t!("Windows 防火墙只应对“专用”网络放行 TCP 4319 与 UDP 5353；安装程序会添加对应规则（需要管理员确认）。").into(),
    ));
    rows
}

const UPDATE_CHANNEL_OPTIONS: &[(&str, &str)] = &[("stable", "正式版"), ("alpha", "预览版")];

fn release_notes_link(rel: &Release) -> Option<View> {
    HyperlinkButton::new()
        .navigate_uri(rel.html_url.clone())
        .ok()
        .map(|b| b.content(t!("查看更新说明")))
}

fn update_status_text(state: &UpdateState) -> String {
    match state {
        UpdateState::Idle => String::new(),
        UpdateState::Checking => t!("检查中…").to_string(),
        UpdateState::UpToDate => t!("已是最新版本").to_string(),
        UpdateState::Available(rel) => tf!("发现新版本 {}", rel.tag),
        UpdateState::Downloading(_) => t!("正在下载并校验…").to_string(),
        UpdateState::Failed(msg) => tf!("检查失败：{}", msg),
    }
}

/// Single-line, length-capped failure text for the banner.
fn short_error(msg: &str) -> String {
    let one_line = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = one_line.chars();
    let head: String = it.by_ref().take(80).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn update_action_label(error: Option<&str>) -> &'static str {
    if error.is_some() {
        t!("重试")
    } else {
        t!("立即更新")
    }
}

fn update_status_row(
    theme: &Theme,
    state: &UpdateState,
    error: Option<&str>,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let busy = matches!(state, UpdateState::Checking | UpdateState::Downloading(_));
    let mut controls: Vec<View> = Vec::new();
    // Idle has no status text; an empty TextBlock would still cost a spacing
    // gap and push the button off the column every other control sits on.
    let status = update_status_text(state);
    if !status.is_empty() {
        controls.push(
            TextBlock::new()
                .text(status)
                .font_size(theme.body_size)
                .max_width(360.0)
                .text_wrapping(windows_reactor::TextWrapping::Wrap)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    let check_at = controls.len();
    controls.push(
        Button::new()
            .is_enabled(!busy)
            .on_click(ctx.callback(|_| Msg::CheckUpdate { manual: true }))
            .content(t!("立即检查")),
    );
    if let UpdateState::Available(rel) = state {
        if let Some(e) = error {
            controls.insert(
                check_at,
                TextBlock::new()
                    .text(tf!("更新失败：{}", short_error(e)))
                    .font_size(theme.label_size)
                    .foreground(theme.danger)
                    .max_width(360.0)
                    .text_wrapping(windows_reactor::TextWrapping::Wrap)
                    .vertical_alignment(VerticalAlignment::Center)
                    .into(),
            );
        }
        controls.push(
            Button::new()
                .on_click(ctx.callback(|_| Msg::StartUpdate))
                .content(update_action_label(error)),
        );
        controls.extend(release_notes_link(rel));
    }
    setting_row(
        theme,
        "更新状态",
        None,
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(12.0)
            .vertical_alignment(VerticalAlignment::Center)
            .keyed_children(keyed(controls)),
    )
}

/// Slim strip above the page: new version + one-click update. Lives in the
/// chrome row, outside the animated page layers.
pub fn update_banner(
    theme: &Theme,
    state: &UpdateState,
    rel: &Release,
    error: Option<&str>,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let mut actions: Vec<View> = Vec::new();
    if matches!(state, UpdateState::Downloading(_)) {
        actions.push(
            TextBlock::new()
                .text(t!("正在下载并校验…"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    } else {
        actions.push(
            Button::new()
                .on_click(ctx.callback(|_| Msg::StartUpdate))
                .content(update_action_label(error)),
        );
        actions.extend(release_notes_link(rel));
    }
    actions.push(
        Button::new()
            .on_click(ctx.callback(|_| Msg::DismissUpdateBanner))
            .content(SymbolIcon::new().symbol(Symbol::Cancel)),
    );
    // A grid, not a horizontal stack: inside a stack the error text measured at
    // its full width, so a long message pushed the update buttons out of the
    // banner instead of trimming. The star column clips it with an ellipsis.
    let mut left: Vec<View> = vec![
        Border::new()
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center)
            .content(SymbolIcon::new().symbol(Symbol::Download)),
        Border::new()
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .content(
                TextBlock::new()
                    .text(tf!("发现新版本 {}", rel.tag))
                    .font_size(theme.body_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .vertical_alignment(VerticalAlignment::Center),
            ),
    ];
    if let Some(e) = error.filter(|_| matches!(state, UpdateState::Available(_))) {
        left.push(
            Border::new()
                .grid_column(2)
                .vertical_alignment(VerticalAlignment::Center)
                .content(
                    TextBlock::new()
                        .text(tf!("更新失败：{}", short_error(e)))
                        .font_size(theme.label_size)
                        .foreground(theme.danger)
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .vertical_alignment(VerticalAlignment::Center),
                ),
        );
    }
    let left_grid = Grid::new()
        .columns([GridLength::Auto, GridLength::Auto, GridLength::STAR])
        .column_spacing(8.0)
        .keyed_children(keyed(left));
    let base = theme
        .page_bg
        .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground));
    let banner_base: View = Border::new()
        .background(base)
        .corner_radius(CornerRadius::uniform(theme.radius))
        .into();
    let banner_bg: View = Border::new()
        .background(theme.card_bg)
        .corner_radius(CornerRadius::uniform(theme.radius))
        .into();
    let banner_frame: View = Border::new()
        .border_brush(theme.card_border)
        .border_thickness(theme.card_border_thickness())
        .corner_radius(CornerRadius::uniform(theme.radius))
        .padding(Thickness::xy(theme.pad, 6.0))
        .content(
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .children([
                    cell(
                        0,
                        Border::new()
                            .vertical_alignment(VerticalAlignment::Center)
                            .content(left_grid),
                    ),
                    cell(
                        1,
                        StackPanel::new()
                            .orientation(Orientation::Horizontal)
                            .spacing(8.0)
                            .keyed_children(keyed(actions)),
                    ),
                ]),
        );
    Grid::new()
        .margin(Thickness::new(24.0, 8.0, 24.0, 0.0))
        .children([banner_base, banner_bg, banner_frame])
}

/// Shell state the Settings page renders besides `UiConfig`.
pub struct SettingsArgs<'a> {
    pub update: &'a UpdateState,
    pub update_error: Option<&'a str>,
    pub external_interface_enabled: bool,
    pub external_interface_error: Option<&'a str>,
    pub share: &'a crate::devices::ShareState,
    pub fx: globaltokentracker_core::fx::FxStatus,
    pub fx_refreshing: bool,
    pub fx_error: Option<&'a str>,
}

pub fn settings_page(
    config: &UiConfig,
    args: &SettingsArgs,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
    let SettingsArgs {
        update,
        update_error,
        external_interface_enabled,
        external_interface_error,
        share,
        ..
    } = *args;
    let fx = &args.fx;
    let share_rows = share_rows(config, share, theme, ctx);
    let version_row = setting_row(
        theme,
        "当前版本",
        None,
        TextBlock::new()
            .text(globaltokentracker_core::update::current_version().to_string())
            .font_size(theme.body_size)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    );
    let channel_row = setting_row(
        theme,
        "更新渠道",
        Some("预览版包含尚未正式发布的新功能，可能不稳定"),
        setting_dropdown(
            UPDATE_CHANNEL_OPTIONS,
            match config.update_channel.as_str() {
                "" => "stable",
                v => v,
            },
            ctx,
            Msg::SetUpdateChannel,
        ),
    );
    let auto_row = setting_row(
        theme,
        "自动检查更新",
        Some("启动时及每 24 小时检查一次"),
        switch(theme, config.update_auto, ctx.callback(Msg::SetUpdateAuto)),
    );
    let status_row = update_status_row(theme, update, update_error, ctx);

    let theme_row = setting_row(
        theme,
        "主题模式",
        None,
        setting_dropdown(
            THEME_OPTIONS,
            match config.window_theme.as_str() {
                "" => "system",
                v => v,
            },
            ctx,
            Msg::SetThemeMode,
        ),
    );
    let accent_row = setting_row(
        theme,
        "主题色",
        None,
        setting_dropdown(
            ACCENT_OPTIONS,
            config.theme.accent.as_deref().unwrap_or(""),
            ctx,
            Msg::SetAccent,
        ),
    );
    let font_row = setting_row(
        theme,
        "字体",
        Some("仅作用于图表文字；界面控件字体跟随系统"),
        font_dropdown(config.theme.font_family.as_deref().unwrap_or(""), ctx),
    );
    let scale_row = setting_row(theme, "界面字号", None, size_control(theme, ctx));
    let lang_row = setting_row(
        theme,
        "语言",
        None,
        setting_dropdown(
            LANG_OPTIONS,
            match config.lang.as_str() {
                "" => "zh",
                v => v,
            },
            ctx,
            Msg::SetLang,
        ),
    );
    let currency_row = setting_row(
        theme,
        "显示货币",
        Some("金额按缓存汇率换算展示，账本始终以 USD 记录"),
        setting_dropdown(CURRENCY_OPTIONS, &fx.display, ctx, |v| {
            Msg::SetDisplayCurrency(v.to_string())
        }),
    );
    let fx_note = match &fx.source {
        Some(src) => format!(
            "{} · 1 USD = {} {}{}",
            src,
            globaltokentracker_core::viewmodel::fmt::trim_f(fx.usd_to, 4),
            fx.display,
            if fx.stale {
                format!(" · {}", tr("缓存已过期"))
            } else {
                String::new()
            }
        ),
        None => tr("未获取到汇率，按 USD 显示").to_string(),
    };
    let sync_stamp = fx
        .fetched_at_ms
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(|t| {
            tf!(
                "同步于 {}",
                t.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M")
            )
        })
        .unwrap_or_else(|| t!("尚未同步").to_string());
    let mut fx_content: Vec<View> = vec![
        TextBlock::new()
            .text(fx_note)
            .font_size(theme.body_size)
            .text_wrapping(TextWrapping::Wrap)
            .into(),
        TextBlock::new()
            .text(sync_stamp)
            .font_size(theme.label_size)
            .foreground(theme.subtle)
            .into(),
        Button::new()
            .is_enabled(!args.fx_refreshing)
            .on_click(ctx.callback(|_| Msg::RefreshFx))
            .content(if args.fx_refreshing {
                t!("正在同步…")
            } else {
                t!("立即同步")
            }),
    ];
    if let Some(error) = args.fx_error {
        fx_content.push(
            TextBlock::new()
                .text(format!("{} · {error}", t!("同步失败，继续使用缓存")))
                .foreground(theme.danger)
                .font_size(theme.label_size)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    let fx_row = setting_row(theme, "汇率来源", None, vstack(6.0, fx_content));
    let autostart_row = setting_row(
        theme,
        "开机自启动",
        Some("登录 Windows 后自动启动（最小化到托盘）"),
        switch(theme, config.autostart, ctx.callback(Msg::SetAutostart)),
    );
    let close_row = setting_row(
        theme,
        "点击关闭按钮时",
        None,
        setting_dropdown(
            CLOSE_OPTIONS,
            &config.close_action,
            ctx,
            Msg::SetCloseAction,
        ),
    );

    let interface_row = setting_row(
        theme,
        "外部协同接口",
        Some("关闭后停用协同查询和新模型调度，不影响用量统计；已发出的模型请求允许完成"),
        switch(
            theme,
            external_interface_enabled,
            ctx.callback(Msg::SetExternalInterface),
        ),
    );
    let mut interface_rows = vec![interface_row];
    if let Some(error) = external_interface_error {
        interface_rows.push(setting_row(
            theme,
            "接口状态",
            None,
            TextBlock::new()
                .text(tr(error))
                .font_size(theme.body_size)
                .foreground(theme.danger)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        ));
    }

    vec![
        header(theme, tr("设置"), vec![]),
        w::section(
            theme,
            Symbol::FontColor,
            tr("外观"),
            vec![w::card(
                theme,
                vstack(theme.gap, vec![theme_row, accent_row, font_row, scale_row]),
            )],
        ),
        w::section(
            theme,
            Symbol::Setting,
            tr("通用"),
            vec![w::card(
                theme,
                vstack(
                    theme.gap,
                    vec![lang_row, currency_row, fx_row, autostart_row, close_row],
                ),
            )],
        ),
        w::section(
            theme,
            Symbol::Setting,
            tr("外部协同"),
            vec![w::card(theme, vstack(theme.gap, interface_rows))],
        ),
        w::section(
            theme,
            Symbol::Permissions,
            tr("局域网共享"),
            vec![w::card(theme, vstack(theme.gap, share_rows))],
        ),
        w::section(
            theme,
            Symbol::Sync,
            tr("更新"),
            vec![w::card(
                theme,
                vstack(
                    theme.gap,
                    vec![version_row, channel_row, auto_row, status_row],
                ),
            )],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(plan: Plan) -> Vec<usize> {
        plan.iter().map(|(logical, _)| *logical).collect()
    }

    fn fixed(plan: Plan) -> f64 {
        plan.iter().filter_map(|(_, w)| *w).sum()
    }

    #[test]
    fn detail_table_sheds_columns_as_the_row_narrows() {
        // Not measured yet → the widest plan (the ruler arrives a frame later).
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 0.0, 1.0)),
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 900.0, 1.0)),
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        // Cache goes first, then duration, then the tool column, then speed.
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 700.0, 1.0)),
            [0, 1, 2, 3, 4, 6, 8]
        );
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 600.0, 1.0)),
            [0, 2, 3, 4, 6, 8]
        );
        assert_eq!(kept(pick_plan(&DETAIL_PLANS, 500.0, 1.0)), [0, 2, 3, 4, 6]);
        // Nothing fits → the narrowest plan, never an empty layout.
        assert_eq!(kept(pick_plan(&DETAIL_PLANS, 200.0, 1.0)), [0, 2, 3, 4, 6]);
        // A larger body font makes the fixed columns wider: the same 660-wide
        // row that keeps 7 columns at 12pt drops to 5 at 17pt (k ≈ 1.42).
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 660.0, 1.0)),
            [0, 1, 2, 3, 4, 6, 8]
        );
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 660.0, 17.0 / 12.0)),
            [0, 2, 3, 4, 6]
        );
    }

    #[test]
    fn a_picked_plan_always_leaves_the_star_column_its_minimum() {
        for plans in [&DETAIL_PLANS[..], &PRICE_PLANS[..]] {
            let narrowest = plans[plans.len() - 1];
            for k in [1.0, 1.25, 17.0 / 12.0] {
                for width in (300..1400).step_by(20) {
                    let width = f64::from(width);
                    let p = pick_plan(plans, width, k);
                    if !std::ptr::eq(p, narrowest) {
                        assert!(
                            fixed(p) * k + ROW_PAD + STAR_MIN <= width,
                            "width {width} k {k}: fixed {} leaves too little",
                            fixed(p)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_price_row_tooltip_lays_out_every_sources_say() {
        use globaltokentracker_core::pricing::consensus::Stance;
        use globaltokentracker_core::store::PriceQuote;
        let quote = |source: &str, key: &str, input: f64, output: f64, stance| PriceQuote {
            source: source.into(),
            key: key.into(),
            input,
            output,
            cache_read: 0.0,
            stance,
        };
        let row = PriceRow::new(
            "claude-opus-4-6".into(),
            (5.0, 25.0, 0.5, 0.0),
            "portkey".into(),
            2,
            3,
            vec![
                quote("portkey", "claude-opus-4-6", 5.0, 25.0, Stance::Agrees),
                quote("openrouter", "claude-opus-4.6", 5.0, 25.0, Stance::Agrees),
                quote("litellm", "claude-opus-4-6", 6.0, 30.0, Stance::Dissents),
                quote("models.dev", "claude-opus-4-6", 0.0, 0.0, Stance::NoData),
                quote("seed", "claude-opus-4-6", 5.0, 25.0, Stance::Ignored),
            ],
        );
        let tip = price_tooltip(&row);
        let lines: Vec<&str> = tip.lines().collect();
        assert_eq!(lines[0], "claude-opus-4-6");
        assert!(
            lines[1].starts_with("5 / 25") && lines[1].contains("0.5"),
            "{tip}"
        );
        assert!(
            lines[2].contains("portkey") && lines[2].contains("2/3"),
            "{tip}"
        );
        // Backing quotes ✓, the dissenter ✗, no price –, the unvoting seed ·.
        let line = |src: &str| {
            lines
                .iter()
                .find(|l| l.contains(src))
                .copied()
                .unwrap_or("")
        };
        assert!(line("portkey  ").starts_with('✓'), "{tip}");
        assert!(line("openrouter").starts_with('✓'), "{tip}");
        assert!(line("openrouter").contains("[claude-opus-4.6]"), "{tip}");
        assert!(
            line("litellm").starts_with('✗') && line("litellm").contains("6 / 30"),
            "{tip}"
        );
        assert!(line("models.dev").starts_with('–'), "{tip}");
        assert!(
            line("seed").starts_with('·') && line("seed").contains("未计票"),
            "{tip}"
        );
        assert!(row.disputed());
    }

    #[test]
    fn plan_layout_maps_logical_columns_to_grid_columns() {
        let (cols, at) = plan_layout::<9>(DETAIL_PLANS[1], 1.0);
        assert_eq!(cols.len(), 8);
        // Cache (logical 5) is hidden; cost/duration/speed slide left by one.
        assert_eq!(
            at,
            [
                Some(0),
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                None,
                Some(5),
                Some(6),
                Some(7)
            ]
        );
        // Pixel widths scale with the font; the star column stays a star.
        let (wide, _) = plan_layout::<9>(DETAIL_PLANS[1], 2.0);
        assert!(matches!(wide[0], GridLength::Pixel(w) if (w - 168.0).abs() < 1e-9));
        assert_eq!(wide[2], GridLength::STAR);
    }

    #[test]
    fn high_res_wide_table_plans_expand_columns() {
        let (wide_cols, wide_at) = plan_layout::<9>(DETAIL_PLAN_WIDE, 1.0);
        assert_eq!(wide_cols.len(), 9);
        assert_eq!(wide_at[0], Some(0)); // time
        assert_eq!(wide_at[2], Some(2)); // model (STAR)
        assert_eq!(wide_at[8], Some(8)); // speed
        // Time column is 136 DIP in the wide plan (vs 90 DIP in normal plans).
        assert_eq!(wide_cols[0], GridLength::Pixel(136.0));
        assert_eq!(wide_cols[2], GridLength::STAR);

        let (price_cols, price_at) = plan_layout::<6>(PRICE_PLAN_WIDE, 1.0);
        assert_eq!(price_cols.len(), 6);
        assert_eq!(price_at[0], Some(0)); // model (STAR)
        assert_eq!(price_cols[0], GridLength::STAR);
        assert_eq!(price_cols[1], GridLength::Pixel(112.0));
        assert_eq!(price_cols[5], GridLength::Pixel(136.0));
    }
}
