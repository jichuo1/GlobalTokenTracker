//! Widget registry — each dashboard block is an addressable widget with an id,
//! title and icon. `UiConfig` orders/hides them; pages render them via `render`.
//! Adding a widget = one registry entry + one match arm.

use crate::gpu_slide::{MAX_SLIDE, Slide};
use crate::i18n::tr;
use crate::theme::Theme;
use crate::{Msg, Shell, t, tf};
use globaltokentracker_core::viewmodel::fmt;
use std::cell::Cell;
use std::rc::Rc;
use windows_canvas::Invalidator;
use windows_reactor::*;

/// (id, title, icon) — stable ids persisted in ui.json.
pub const OVERVIEW_WIDGETS: &[(&str, &str, Symbol)] = &[
    ("stats", "统计卡", Symbol::Calculator),
    ("trend", "近 30 天趋势", Symbol::FourBars),
    ("heat", "活跃热力图", Symbol::Calendar),
    ("share", "占比分布", Symbol::AllApps),
    ("apps", "本周 · 按工具", Symbol::List),
    ("quotas", "订阅配额", Symbol::Clock),
    ("unpriced", "未计价提示", Symbol::Important),
];

pub fn widget_title(id: &str) -> &'static str {
    OVERVIEW_WIDGETS
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, t, _)| tr(t))
        .unwrap_or(tr("部件"))
}

pub fn widget_icon(id: &str) -> Symbol {
    OVERVIEW_WIDGETS
        .iter()
        .find(|(i, _, _)| *i == id)
        .map(|(_, _, s)| *s)
        .unwrap_or(Symbol::Placeholder)
}

pub fn registry_ids() -> Vec<&'static str> {
    OVERVIEW_WIDGETS.iter().map(|(id, _, _)| *id).collect()
}

// ------------------------------------------------------------------ slide

/// Wrap each top-level page block so it can be moved on its own. The wrapper
/// is always a `Grid` (rest and flight alike) so nothing remounts when a
/// flight starts or ends — canvas swapchains would flash.
///
/// The motion itself is compositor-driven (`gpu_slide`): the first
/// `MAX_SLIDE` wrappers of a page are bound to `LayerHost`s whose Visuals get
/// key-frame animations when that page enters. Layout is never touched.
pub fn slide_children(children: Vec<View>, slide: &Slide) -> Vec<View> {
    children
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            let mut wrap = Grid::new();
            if i < MAX_SLIDE {
                wrap = wrap.element_ref(&slide.hosts[i].r);
            }
            wrap.keyed_children([KeyedView::new("c", v)])
        })
        .collect()
}

/// Card chrome: border + background + optional accent edge. All skin values
/// come from `theme` — a skin swap restyles every card at once.
pub fn card(theme: &Theme, content: View) -> View {
    Border::new()
        .background(theme.card_bg)
        .border_brush(theme.card_border)
        .border_thickness(theme.card_border_thickness())
        .corner_radius(CornerRadius::uniform(theme.radius))
        .padding(Thickness::uniform(theme.pad))
        .content(content)
}

/// Icon + title + hairline rule — the line is the visual divider the design
/// asks for; it stretches to fill remaining width.
pub fn section_header(theme: &Theme, icon: Symbol, title: &str) -> View {
    // The rule takes whatever the icon + title leave (a fixed-width rule ran
    // past narrow cards and stopped short in wide ones).
    Grid::new()
        .columns([GridLength::Auto, GridLength::Auto, GridLength::STAR])
        .column_spacing(8.0)
        .children([
            Border::new()
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center)
                .content(SymbolIcon::new().symbol(icon)),
            Border::new().grid_column(1).content(
                TextBlock::new()
                    .text(title)
                    .font_size(theme.h2_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .vertical_alignment(VerticalAlignment::Center),
            ),
            Border::new()
                .grid_column(2)
                .height(1.0)
                .background(theme.divider)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        ])
}

/// Emphasis tones — used sparingly: only state signals earn color.
#[derive(Clone, Copy)]
pub enum BadgeTone {
    Accent,
    Warn,
    Danger,
    Muted,
}

/// Outline-style pill: 1px tone border, tinted text, transparent fill.
/// Compact and readable in both light/dark skins.
pub fn badge(theme: &Theme, text: String, tone: BadgeTone) -> View {
    let brush = match tone {
        BadgeTone::Accent => theme.accent,
        BadgeTone::Warn => theme.warn,
        BadgeTone::Danger => theme.danger,
        BadgeTone::Muted => theme.subtle,
    };
    Border::new()
        .corner_radius(CornerRadius::uniform(10.0))
        .border_brush(brush)
        .border_thickness(Thickness::uniform(1.0))
        .padding(Thickness::xy(7.0, 1.0))
        .content(
            TextBlock::new()
                .text(text)
                .font_size(theme.label_size)
                .foreground(brush),
        )
}

/// A single stat tile: icon + label + big number + sub-line.
/// `emph` paints the value in accent and `tag` pins a pill next to the label —
/// reserve both for the metric that carries the page (today's estimated cost).
pub fn stat_card(
    theme: &Theme,
    icon: Symbol,
    label: &str,
    value: String,
    sub: String,
    emph: bool,
    tag: Option<(&str, BadgeTone)>,
) -> View {
    let mut label_children: Vec<View> = vec![
        SymbolIcon::new().symbol(icon).into(),
        TextBlock::new()
            .text(label)
            .font_size(theme.label_size)
            .foreground(theme.subtle)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ];
    if let Some((t, tone)) = tag {
        label_children.push(badge(theme, t.to_string(), tone));
    }
    let mut value_tb = TextBlock::new()
        .text(value)
        .font_size(26.0)
        .font_weight(FontWeight::SEMI_BOLD);
    if emph {
        value_tb = value_tb.foreground(theme.accent);
    }
    card(
        theme,
        StackPanel::new()
            .orientation(Orientation::Vertical)
            .spacing(4.0)
            .children((
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(6.0)
                    .keyed_children(crate::pages::keyed(label_children)),
                value_tb,
                TextBlock::new()
                    .text(sub)
                    .font_size(theme.label_size)
                    .foreground(theme.accent_soft),
            )),
    )
}

/// Shared trend-hover state: pointer callbacks write `hover` via a Msg round
/// trip; the D2D draw closure reads it every invalidated frame. `width`/`count`
/// are written by the draw pass so hover math uses the real surface size.
/// `tip` is the delayed-tooltip index — armed by `TrendTip` after the pointer
/// has dwelled on one bar (~450ms), cleared on move/leave.
#[derive(Default)]
pub struct TrendShared {
    pub hover: Cell<Option<usize>>,
    pub tip: Cell<Option<usize>>,
    /// Bar the dwell timer is armed for — moving bars rearms it.
    pub pending: Cell<Option<usize>>,
    pub width: Cell<f32>,
    pub count: Cell<usize>,
}

/// Owned by `Shell`; cloned handles flow into the widget each render.
#[derive(Clone)]
pub struct TrendHandle {
    pub shared: Rc<TrendShared>,
    pub inv: Invalidator,
}

impl Default for TrendHandle {
    fn default() -> Self {
        Self {
            shared: Rc::new(TrendShared::default()),
            inv: Invalidator::new(),
        }
    }
}

/// Per-donut hover state — pointer callbacks write it via a Msg round trip,
/// the draw pass reads it every invalidated frame.
#[derive(Default)]
pub struct DonutShared {
    pub hover: Cell<Option<usize>>,
}

/// Owned by `Shell`; one per share-grid column, cloned into each render.
#[derive(Clone)]
pub struct DonutHandle {
    pub shared: Rc<DonutShared>,
    pub inv: Invalidator,
}

impl Default for DonutHandle {
    fn default() -> Self {
        Self {
            shared: Rc::new(DonutShared::default()),
            inv: Invalidator::new(),
        }
    }
}

/// 30-day token trend — Direct2D demand canvas: rounded bars (today at full
/// alpha, history softened), faint mid gridline + hairline baseline, sparse
/// date ticks and a max label drawn by DirectWrite — `theme.font_family` is
/// honored here (the one place family config takes effect on 0.100.0).
/// Pointer hover lifts the bar to full alpha and prints its date·tokens;
/// dwelling ~450ms opens a tooltip card with events/cost/top-3 models.
///
/// `defer`: not this chart's turn to mount yet — see `defer_slot`.
pub fn trend_strip(
    theme: &Theme,
    daily: &[globaltokentracker_core::viewmodel::TrendBucket],
    trend: &TrendHandle,
    defer: bool,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let days: Vec<globaltokentracker_core::viewmodel::TrendBucket> =
        daily.iter().rev().take(60).rev().cloned().collect();
    let accent = theme.accent_cf;
    let subtle = theme.subtle_cf;
    let divider = theme.divider_cf;
    let card_bg = theme.card_cf;
    let family = theme.font_family.clone();
    let label_pt = theme.label_size as f32;
    let shared = trend.shared.clone();
    Border::new()
        .height(160.0)
        // Null Background = XAML skips hit-testing entirely; Transparent keeps
        // the canvas invisible yet receives PointerMoved/Exited.
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
        .on_pointer_moved(ctx.callback(|e: PointerEventInfo| Msg::TrendHover(e.x)))
        .on_pointer_exited(ctx.callback(|_| Msg::TrendLeave))
        .content(defer_slot(
            defer,
            windows_canvas::canvas_invalidated(&trend.inv, move |ctx| {
                use windows_canvas::{ColorF, Rect, TextAlignment, TextFormat, Vector2};
                // Clear before the early return — an empty `days` must still wipe
                // the previous frame, otherwise stale bars linger after filters.
                // Painted with the card fill: swapchain transparency blends onto
                // the page below, not the card, so TRANSPARENT showed as a dark box.
                ctx.clear(card_bg);
                let (w, h) = (ctx.width, ctx.height);
                if w < 16.0 || h < 24.0 || days.is_empty() {
                    return Ok(());
                }

                let tf = TextFormat::new(&family, label_pt)?;
                let tf_r = tf.clone().with_alignment(TextAlignment::Trailing);
                let ink = ctx.create_solid_brush(subtle)?;
                let line = ctx.create_solid_brush(divider)?;

                // Layout: top label strip, plot area, bottom ticks. The strips
                // are 18 / 16px at the default 11pt label and grow with the
                // font — fixed heights clipped the date ticks at larger sizes.
                let top = (label_pt * 1.65).max(18.0).ceil();
                let strip = (label_pt * 1.5).max(16.0).ceil();
                let bottom = h - strip;
                let plot_h = (bottom - top).max(1.0);
                let max = days.iter().map(|d| d.tokens).max().unwrap_or(1).max(1) as f32;

                // Max label (top-left) + faint mid gridline.
                ctx.draw_text(
                    &fmt::tokens_exact(max as u64),
                    &tf,
                    &Rect::new(0.0, 0.0, 160.0, top),
                    &ink,
                );
                let mid_y = top + plot_h * 0.5;
                ctx.draw_line(Vector2::new(0.0, mid_y), Vector2::new(w, mid_y), &line, 1.0);
                ctx.draw_line(
                    Vector2::new(0.0, bottom),
                    Vector2::new(w, bottom),
                    &line,
                    1.0,
                );

                let n = days.len() as f32;
                let slot = w / n;
                let bar_w = (slot * 0.62).clamp(3.0, 20.0);
                let last = days.len() - 1;
                let hover = shared.hover.get().filter(|&i| i <= last);
                // GTT_TIPTEST=<idx> forces the tooltip in test builds — injected
                // pointer input never reaches WinUI3's content island, so this is
                // the screenshot-verifiable path for the popup itself.
                let tip = shared
                    .tip
                    .get()
                    .or_else(|| {
                        std::env::var("GTT_TIPTEST")
                            .ok()
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .filter(|&i| i <= last);
                shared.width.set(w);
                shared.count.set(days.len());
                for (i, d) in days.iter().enumerate() {
                    let bh = ((d.tokens as f32) / max * plot_h).max(if d.tokens > 0 {
                        3.0
                    } else {
                        1.5
                    });
                    let x = slot * i as f32 + (slot - bar_w) * 0.5;
                    let lit = i == last || hover == Some(i);
                    let brush = ctx.create_solid_brush(ColorF::new(
                        accent.r,
                        accent.g,
                        accent.b,
                        accent.a * if lit { 1.0 } else { 0.45 },
                    ))?;
                    let bar = windows_canvas::RoundedRect::new(
                        Rect::new(x, bottom - bh, x + bar_w, bottom),
                        2.5,
                        2.5,
                    );
                    ctx.fill_rounded_rect(&bar, &brush);
                    if hover == Some(i) {
                        ctx.draw_rounded_rect(&bar, &ink, 1.0);
                        // Hover detail top-right: "MM-DD · 1,234 tok".
                        ctx.draw_text(
                            &tf!(
                                "{} · {} tok",
                                d.date.get(5..10).unwrap_or(&d.date),
                                fmt::tokens_exact(d.tokens)
                            ),
                            &tf_r,
                            &Rect::new(w - 220.0, 0.0, w, top),
                            &ink,
                        );
                    }
                }

                // Delayed tooltip card — drawn last so it floats above the plot.
                if let Some(i) = tip {
                    let d = &days[i];
                    let day_label = if d.date.len() > 10 {
                        d.date.clone()
                    } else {
                        d.date.get(5..10).unwrap_or(&d.date).to_string()
                    };
                    let mut lines: Vec<String> = vec![
                        tf!(
                            "{} tok · {}",
                            fmt::tokens_exact(d.tokens),
                            fmt::usd(d.cost_usd)
                        ),
                        tf!("{} 事件", fmt::tokens_exact(d.events)),
                    ];
                    for (m, t) in &d.top {
                        lines.push(format!(
                            "{}  {}",
                            if m.chars().count() > 20 {
                                tf!("{}…", m.chars().take(19).collect::<String>())
                            } else {
                                m.clone()
                            },
                            fmt::tokens_exact(*t)
                        ));
                    }
                    let line_h = label_pt + 4.0;
                    let pw = 216.0f32;
                    let ph = 26.0 + lines.len() as f32 * line_h + 10.0;
                    let px = (slot * i as f32 + slot * 0.5 - pw * 0.5)
                        .clamp(4.0, (w - pw - 4.0).max(4.0));
                    let py = top + 2.0;
                    let panel = windows_canvas::RoundedRect::new(
                        Rect::new(px, py, px + pw, py + ph),
                        7.0,
                        7.0,
                    );
                    // Near-opaque dark card (Fluent tooltip idiom; reads on both themes).
                    let bg = ctx.create_solid_brush(ColorF::from_rgba8(28, 28, 30, 242))?;
                    let frame = ctx.create_solid_brush(ColorF::from_rgba8(255, 255, 255, 36))?;
                    let head = ctx.create_solid_brush(accent)?;
                    let body = ctx.create_solid_brush(ColorF::from_rgba8(235, 235, 235, 255))?;
                    ctx.fill_rounded_rect(&panel, &bg);
                    ctx.draw_rounded_rect(&panel, &frame, 1.0);
                    ctx.draw_text(
                        &day_label,
                        &tf,
                        &Rect::new(px + 10.0, py + 7.0, px + pw - 10.0, py + 7.0 + line_h),
                        &head,
                    );
                    for (li, l) in lines.iter().enumerate() {
                        let y = py + 7.0 + (li + 1) as f32 * line_h;
                        ctx.draw_text(
                            l,
                            &tf,
                            &Rect::new(px + 10.0, y, px + pw - 10.0, y + line_h),
                            &body,
                        );
                    }
                }

                // Sparse date ticks: first / last day (MM-DD tail of ISO date).
                let tick = |d: &str| d.get(5..10).unwrap_or(d).to_string();
                ctx.draw_text(
                    &tick(&days[0].date),
                    &tf,
                    &Rect::new(0.0, bottom + 2.0, 80.0, h),
                    &ink,
                );
                ctx.draw_text(
                    &tick(&days[last].date),
                    &tf_r,
                    &Rect::new(w - 80.0, bottom + 2.0, w, h),
                    &ink,
                );
                Ok(())
            }),
        ))
}

/// Where the heatmap grid sits inside its canvas — written by the draw pass,
/// read by pointer handlers so hit-testing uses the real (measured) layout.
#[derive(Clone, Copy, Default)]
pub struct HeatGeom {
    pub x0: f32,
    pub y0: f32,
    pub pitch: f32,
    pub cols: usize,
    pub count: usize,
}

/// Heatmap hover state — same round trip as `TrendShared`: `hover` is the day
/// index under the pointer, `tip` the dwell-armed tooltip index.
#[derive(Default)]
pub struct HeatShared {
    pub hover: Cell<Option<usize>>,
    pub tip: Cell<Option<usize>>,
    pub pending: Cell<Option<usize>>,
    pub geom: Cell<HeatGeom>,
}

impl HeatShared {
    /// Day index (`col * 7 + row`, Monday-first) under canvas point `(x, y)`.
    pub fn cell_at(&self, x: f32, y: f32) -> Option<usize> {
        let g = self.geom.get();
        if g.pitch <= 0.0 || x < g.x0 || y < g.y0 {
            return None;
        }
        let (col, row) = (
            ((x - g.x0) / g.pitch) as usize,
            ((y - g.y0) / g.pitch) as usize,
        );
        let idx = col * 7 + row;
        (col < g.cols && row < 7 && idx < g.count).then_some(idx)
    }
}

#[derive(Clone)]
pub struct HeatHandle {
    pub shared: Rc<HeatShared>,
    pub inv: Invalidator,
}

impl Default for HeatHandle {
    fn default() -> Self {
        Self {
            shared: Rc::new(HeatShared::default()),
            inv: Invalidator::new(),
        }
    }
}

const HEAT_CELL_MAX: f32 = 14.0;
const HEAT_CELL_MIN: f32 = 6.0;
const HEAT_GAP: f32 = 3.0;
/// Quartile-level alpha of the accent colour (level 0 is the divider colour).
const HEAT_ALPHA: [f32; 4] = [0.30, 0.52, 0.76, 1.0];

/// Strips above/below the grid, scaled with the label font like the trend's.
fn heat_strips(label_pt: f32) -> (f32, f32) {
    let top = (label_pt * 1.65).max(18.0).ceil();
    let bottom = (label_pt * 1.6).max(18.0).ceil() + 6.0;
    (top, bottom)
}

/// Canvas height for the nominal cell size; narrower windows shrink the cells
/// and the grid is centred vertically in this box.
pub fn heat_height(label_pt: f64) -> f64 {
    let (top, bottom) = heat_strips(label_pt as f32);
    f64::from(top + bottom + 7.0 * (HEAT_CELL_MAX + HEAT_GAP)) + 4.0
}

/// GitHub-style activity heatmap: 7 rows (Mon–Sun) × one column per week, one
/// Direct2D canvas. Cell colour = accent alpha by quartile level of `metric`
/// over the window's non-zero days; hover outlines a cell and dwelling opens a
/// tooltip with all four metrics. `defer`: see `defer_slot`.
pub fn heatmap(
    theme: &Theme,
    days: &[globaltokentracker_core::viewmodel::HeatDay],
    metric: crate::heat::HeatMetric,
    handle: &HeatHandle,
    defer: bool,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let days: Vec<globaltokentracker_core::viewmodel::HeatDay> = days.to_vec();
    let accent = theme.accent_cf;
    let subtle = theme.subtle_cf;
    let divider = theme.divider_cf;
    let card_bg = theme.card_cf;
    let family = theme.font_family.clone();
    let label_pt = theme.label_size as f32;
    let shared = handle.shared.clone();
    Border::new()
        .height(heat_height(theme.label_size))
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
        .on_pointer_moved(ctx.callback(|e: PointerEventInfo| Msg::HeatHover(e.x, e.y)))
        .on_pointer_exited(ctx.callback(|_| Msg::HeatLeave))
        .content(defer_slot(
            defer,
            windows_canvas::canvas_invalidated(&handle.inv, move |ctx| {
                use crate::heat;
                use windows_canvas::{ColorF, Rect, RoundedRect, TextAlignment, TextFormat};
                ctx.clear(card_bg);
                let (w, h) = (ctx.width, ctx.height);
                if w < 80.0 || h < 40.0 || days.is_empty() {
                    return Ok(());
                }
                let tf = TextFormat::new(&family, label_pt)?;
                let tf_r = tf.clone().with_alignment(TextAlignment::Trailing);
                let ink = ctx.create_solid_brush(subtle)?;
                let level_brush = |lv: u8| {
                    if lv == 0 {
                        ctx.create_solid_brush(divider)
                    } else {
                        ctx.create_solid_brush(ColorF::new(
                            accent.r,
                            accent.g,
                            accent.b,
                            accent.a * HEAT_ALPHA[usize::from(lv) - 1],
                        ))
                    }
                };

                let cols = days.len().div_ceil(7);
                let (top, bottom) = heat_strips(label_pt);
                let label_w = (label_pt * 2.8).ceil();
                let cell =
                    ((w - label_w) / cols as f32 - HEAT_GAP).clamp(HEAT_CELL_MIN, HEAT_CELL_MAX);
                let pitch = cell + HEAT_GAP;
                let grid_h = 7.0 * pitch - HEAT_GAP;
                let y_off = ((h - (top + grid_h + bottom)) * 0.5).max(0.0);
                let (x0, y0) = (label_w, y_off + top);
                shared.geom.set(HeatGeom {
                    x0,
                    y0,
                    pitch,
                    cols,
                    count: days.len(),
                });

                let t = heat::thresholds(days.iter().map(|d| metric.value(d)));
                let hover = shared.hover.get().filter(|&i| i < days.len());
                // GTT_HEATTEST=<idx> forces the tooltip (see GTT_TIPTEST).
                let tip = shared
                    .tip
                    .get()
                    .or_else(|| {
                        std::env::var("GTT_HEATTEST")
                            .ok()
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .filter(|&i| i < days.len());

                // Month labels over the first column of each month.
                for (c, m) in heat::month_marks(&days) {
                    let x = x0 + c as f32 * pitch;
                    ctx.draw_text(
                        &heat::month_label(m),
                        &tf,
                        &Rect::new(x, y_off, x + 48.0, y_off + top),
                        &ink,
                    );
                }
                // Mon / Wed / Fri.
                for (row, name) in [(0usize, tr("一")), (2, tr("三")), (4, tr("五"))] {
                    let y = y0 + row as f32 * pitch;
                    ctx.draw_text(
                        name,
                        &tf,
                        &Rect::new(0.0, y - 2.0, label_w - 4.0, y + pitch),
                        &ink,
                    );
                }

                for (i, d) in days.iter().enumerate() {
                    let (c, r) = (i / 7, i % 7);
                    let x = x0 + c as f32 * pitch;
                    let y = y0 + r as f32 * pitch;
                    let lv = heat::level(metric.value(d), &t);
                    let brush = level_brush(lv)?;
                    let rr = RoundedRect::new(Rect::new(x, y, x + cell, y + cell), 2.5, 2.5);
                    ctx.fill_rounded_rect(&rr, &brush);
                    if hover == Some(i) {
                        ctx.draw_rounded_rect(&rr, &ink, 1.5);
                    }
                }

                // Bottom row: summary (left), legend (right).
                let by = y0 + grid_h + 8.0;
                let s = heat::summarize(&days, metric);
                let legend_cell = cell.min(12.0);
                let word_w = |s: &str| {
                    s.chars()
                        .map(|c| {
                            if c.is_ascii() {
                                label_pt * 0.6
                            } else {
                                label_pt
                            }
                        })
                        .sum::<f32>()
                };
                let (less, more) = (tr("少"), tr("多"));
                let swatches = 5.0 * (legend_cell + 2.0);
                let legend_w = word_w(less) + swatches + word_w(more) + 12.0;
                ctx.draw_text(
                    &tf!(
                        "近一年 {} 天活跃 · 最长连续 {} 天 · 合计 {}",
                        s.active_days,
                        s.longest_streak,
                        heat::fmt_total(metric, s.total)
                    ),
                    &tf,
                    &Rect::new(x0, by, (w - legend_w - 8.0).max(x0 + 40.0), by + bottom),
                    &ink,
                );
                let mut lx = w - legend_w;
                ctx.draw_text(
                    less,
                    &tf,
                    &Rect::new(lx, by, lx + word_w(less) + 4.0, by + bottom),
                    &ink,
                );
                lx += word_w(less) + 6.0;
                for lv in 0..5u8 {
                    let brush = level_brush(lv)?;
                    let rr = RoundedRect::new(
                        Rect::new(lx, by + 1.0, lx + legend_cell, by + 1.0 + legend_cell),
                        2.0,
                        2.0,
                    );
                    ctx.fill_rounded_rect(&rr, &brush);
                    lx += legend_cell + 2.0;
                }
                ctx.draw_text(more, &tf, &Rect::new(lx + 4.0, by, w, by + bottom), &ink);

                // Delayed tooltip card, drawn last.
                if let Some(i) = tip {
                    let d = &days[i];
                    let lines = [
                        tf!("{} tok", fmt::tokens_exact(d.tokens)),
                        fmt::usd(d.cost_usd),
                        tf!("{} 次调用", fmt::tokens_exact(d.events)),
                        tf!("时长 {}", heat::fmt_span(d.duration_ms)),
                    ];
                    let line_h = label_pt + 4.0;
                    let (pw, ph) = (176.0f32, 26.0 + lines.len() as f32 * line_h + 10.0);
                    let (cx, cy) = (
                        x0 + (i / 7) as f32 * pitch + cell * 0.5,
                        y0 + (i % 7) as f32 * pitch,
                    );
                    let px = (cx - pw * 0.5).clamp(4.0, (w - pw - 4.0).max(4.0));
                    let below = cy + pitch + 4.0;
                    let py = if below + ph <= h {
                        below
                    } else {
                        (cy - ph - 4.0).max(2.0)
                    };
                    let panel = RoundedRect::new(Rect::new(px, py, px + pw, py + ph), 7.0, 7.0);
                    let bg = ctx.create_solid_brush(ColorF::from_rgba8(28, 28, 30, 242))?;
                    let frame = ctx.create_solid_brush(ColorF::from_rgba8(255, 255, 255, 36))?;
                    let head = ctx.create_solid_brush(accent)?;
                    let body = ctx.create_solid_brush(ColorF::from_rgba8(235, 235, 235, 255))?;
                    ctx.fill_rounded_rect(&panel, &bg);
                    ctx.draw_rounded_rect(&panel, &frame, 1.0);
                    ctx.draw_text(
                        &d.date,
                        &tf,
                        &Rect::new(px + 10.0, py + 7.0, px + pw - 10.0, py + 7.0 + line_h),
                        &head,
                    );
                    for (li, l) in lines.iter().enumerate() {
                        let y = py + 7.0 + (li + 1) as f32 * line_h;
                        ctx.draw_text(
                            l,
                            &tf_r,
                            &Rect::new(px + 10.0, y, px + pw - 10.0, y + line_h),
                            &body,
                        );
                    }
                }
                Ok(())
            }),
        ))
}

/// Canvas slot content. Every `canvas_invalidated` builds its *own* GPU device
/// (D3D11 + D2D) on its first layout — ~15ms each on the UI thread, and the
/// library offers no shared-device variant for demand canvases. Overview has
/// five, so mounting them mid-slide was the ~85ms freeze on every flight
/// to/from it. While `defer` (slide in flight, or later in the post-settle
/// stagger) the slot holds an empty placeholder inside the same sized Border;
/// a later frame swaps the real canvas in. Building the canvas `View` itself
/// is cheap — the device only appears once it is mounted.
fn defer_slot(defer: bool, canvas: View) -> View {
    if defer { Border::new().into() } else { canvas }
}

/// Fixed palette for pie slices 1.. (slice 0 always uses the live accent so
/// the dominant share reads in the brand color). Hues chosen to stay legible
/// on both light and dark skins; the gray tail usually lands on 其他.
pub const SLICE_PALETTE: [(u8, u8, u8); 7] = [
    (0x10, 0xa8, 0x74),
    (0xe8, 0x85, 0x3d),
    (0x8b, 0x6f, 0xd8),
    (0xd8, 0x4d, 0x5b),
    (0x5b, 0xb8, 0xd8),
    (0xd8, 0xb8, 0x4d),
    (0x9e, 0x9e, 0x9e),
];

/// XAML-side swatch for legend rows — same index rule as the D2D slice fill.
pub fn slice_brush(theme: &Theme, i: usize) -> Brush {
    if i == 0 {
        return theme.accent;
    }
    let (r, g, b) = SLICE_PALETTE[(i - 1) % SLICE_PALETTE.len()];
    Brush::Solid(Color::rgb(r, g, b))
}

/// Donut canvas — annular sectors approximated by polygon paths (~3° steps;
/// a real arc primitive isn't exposed on 0.100). The ring parks in the left
/// h×h square of a wider box so the hover bubble has room beside it;
/// `slices` arrive pre-folded (top-N + 其他); `center` is the label in the hole.
/// Each donut gets its own Invalidator — sharing one across canvases draws
/// only the first (demand canvases attach once per render anyway).
/// Everything a share-donut needs beyond `theme`/`ctx` — bundles the cell
/// data with its hover wiring so `donut`/`donut_cell` stay under the arg cap.
pub struct DonutSpec<'a> {
    /// Pre-folded slices (top-N + 其他), already zero-filtered.
    pub slices: &'a [(String, f64)],
    /// Label inside the hole (usually the formatted total).
    pub center: String,
    /// Raw-value formatter for legend + hover detail tails.
    pub fmt_v: fn(f64) -> String,
    /// Column index — the Msg key matching `Shell.donuts[key]`.
    pub key: u8,
    /// Hover state + repaint handle owned by `Shell`.
    pub handle: &'a DonutHandle,
    /// Not this donut's turn to mount yet — placeholder (see `defer_slot`).
    pub defer: bool,
}

fn donut(theme: &Theme, spec: DonutSpec<'_>, ctx: &mut ViewContext<Shell>) -> View {
    let DonutSpec {
        slices,
        center,
        fmt_v,
        key,
        handle,
        defer,
    } = spec;
    let slices: Vec<(String, f64)> = slices.to_vec();
    let total: f64 = slices.iter().map(|s| s.1).sum();
    let accent = theme.accent_cf;
    let subtle = theme.subtle_cf;
    // SwapChain pixels composite straight onto the window surface — a
    // TRANSPARENT clear shows the page, not the card beneath. Painting the
    // card's own fill is what makes the canvas indistinguishable from it.
    let card_bg = theme.card_cf;
    let family = theme.font_family.clone();
    let body_pt = theme.body_size as f32;
    let label_pt = theme.label_size as f32;
    let shared = handle.shared.clone();
    // Angular hit-test mirrors the draw geometry — the element is fixed-size
    // so pointer-local coords map straight onto the drawn ring.
    let hit_vals: Vec<f64> = slices.iter().map(|s| s.1).collect();
    // GTT_DONUTTEST=<col>,<idx> forces a hover in test builds — injected
    // pointer input never reaches WinUI3's content island (same workaround
    // as the trend tooltip's GTT_TIPTEST).
    let hover_test: Option<(u8, usize)> = std::env::var("GTT_DONUTTEST").ok().and_then(|v| {
        let (c, i) = v.split_once(',')?;
        Some((c.trim().parse().ok()?, i.trim().parse().ok()?))
    });
    Border::new()
        // Wider than the ring: the band right of it hosts the hover bubble.
        .width(200.0)
        .height(132.0)
        // Left edge aligns with the legend rows below — centering floated
        // the ring right of the text column.
        .horizontal_alignment(HorizontalAlignment::Left)
        // Transparent (not null) Background keeps the canvas hit-testable.
        .background(Brush::Solid(Color::argb(0, 0, 0, 0)))
        .on_pointer_moved(ctx.callback(move |e: PointerEventInfo| {
            Msg::DonutHover(key, donut_hit(e.x, e.y, 200.0, 132.0, &hit_vals))
        }))
        .on_pointer_exited(ctx.callback(move |_| Msg::DonutHover(key, None)))
        .content(defer_slot(
            defer,
            windows_canvas::canvas_invalidated(&handle.inv, move |ctx| {
                use windows_canvas::{
                    ColorF, ParagraphAlignment, PathBuilder, Rect, TextAlignment, TextFormat,
                    Vector2,
                };
                ctx.clear(card_bg);
                let (w, h) = (ctx.width, ctx.height);
                if w < 40.0 || total <= 0.0 {
                    return Ok(());
                }
                // Ring parks inside the left h×h square — `donut_geom` is the
                // single source the pointer hit-test mirrors.
                let (cx, cy, r_out, r_in) = donut_geom(w as f64, h as f64);
                let (cx, cy, r_out, r_in) = (cx as f32, cy as f32, r_out as f32, r_in as f32);
                let hovered = hover_test
                    .filter(|(c, _)| *c == key)
                    .map(|(_, i)| i)
                    .or_else(|| shared.hover.get())
                    .filter(|&i| i < slices.len());
                let gap = if slices.len() > 1 { 0.016f32 } else { 0.0 };
                let mut a = -std::f32::consts::FRAC_PI_2;
                let mut hover_am = None;
                for (i, (_, v)) in slices.iter().enumerate() {
                    let span = (*v / total).max(0.0) as f32 * std::f32::consts::TAU;
                    if hovered == Some(i) {
                        hover_am = Some(a + span * 0.5);
                    }
                    let a1 = a + span;
                    let (a0, a1c) = (a + gap, (a1 - gap).max(a + gap));
                    if a1c > a0 {
                        // Hovered slice pops +2.5px; siblings dim when a hover
                        // is active so the focus reads instantly.
                        let lit = hovered == Some(i);
                        let ro = if lit { r_out + 2.5 } else { r_out };
                        let n =
                            (((a1c - a0) / (std::f32::consts::TAU / 120.0)).ceil() as usize).max(2);
                        let mut pts = Vec::with_capacity(2 * (n + 1));
                        for k in 0..=n {
                            let t = a0 + (a1c - a0) * k as f32 / n as f32;
                            pts.push(Vector2::new(cx + ro * t.cos(), cy + ro * t.sin()));
                        }
                        for k in (0..=n).rev() {
                            let t = a0 + (a1c - a0) * k as f32 / n as f32;
                            pts.push(Vector2::new(cx + r_in * t.cos(), cy + r_in * t.sin()));
                        }
                        let path = PathBuilder::new(ctx.device())?.polygon(pts)?;
                        let mut color = if i == 0 {
                            accent
                        } else {
                            let (r, g, b) = SLICE_PALETTE[(i - 1) % SLICE_PALETTE.len()];
                            ColorF::from_rgb8(r, g, b)
                        };
                        if hovered.is_some() && !lit {
                            color.a *= 0.32;
                        }
                        ctx.fill_path(&path, &ctx.create_solid_brush(color)?);
                    }
                    a = a1;
                }
                let ink = ctx.create_solid_brush(subtle)?;
                // Center always shows the total — hover detail lives in the
                // bubble, which has room the hole never did.
                let tf = TextFormat::new_bold(&family, body_pt)?
                    .with_alignment(TextAlignment::Center)
                    .with_paragraph_alignment(ParagraphAlignment::Center);
                ctx.draw_text(
                    &center,
                    &tf,
                    &Rect::new(cx - r_in + 2.0, cy - r_in, cx + r_in - 2.0, cy + r_in),
                    &ink,
                );
                // Hover bubble anchored on the ring's outer edge at the slice
                // mid-angle, clamped inside the canvas — same Fluent dark
                // card idiom as the trend tooltip.
                if let Some(i) = hovered {
                    let (name, v) = &slices[i];
                    let am = hover_am.unwrap_or(-std::f32::consts::FRAC_PI_2);
                    let ar = r_out + 16.0;
                    let (bx, by) = (cx + ar * am.cos(), cy + ar * am.sin());
                    let (pw, ph) = (150.0f32, 58.0f32);
                    let px = (bx - pw * 0.5).clamp(4.0, (w - pw - 4.0).max(4.0));
                    let py = (by - ph * 0.5).clamp(4.0, (h - ph - 4.0).max(4.0));
                    let panel = windows_canvas::RoundedRect::new(
                        Rect::new(px, py, px + pw, py + ph),
                        7.0,
                        7.0,
                    );
                    let bg = ctx.create_solid_brush(ColorF::from_rgba8(28, 28, 30, 242))?;
                    let frame = ctx.create_solid_brush(ColorF::from_rgba8(255, 255, 255, 36))?;
                    ctx.fill_rounded_rect(&panel, &bg);
                    ctx.draw_rounded_rect(&panel, &frame, 1.0);
                    let short: String = if name.chars().count() > 18 {
                        crate::tf!("{}…", name.chars().take(17).collect::<String>())
                    } else {
                        name.clone()
                    };
                    let detail = format!("{:.1}% · {}", v / total * 100.0, fmt_v(*v));
                    let rank = crate::tf!("第 {} / {} 项", i + 1, slices.len());
                    let tf_name = TextFormat::new_bold(&family, label_pt + 1.0)?;
                    let tf_val = TextFormat::new(&family, label_pt)?;
                    let name_ink = ctx.create_solid_brush(accent)?;
                    let body_ink =
                        ctx.create_solid_brush(ColorF::from_rgba8(235, 235, 235, 255))?;
                    let lh = label_pt + 4.0;
                    ctx.draw_text(
                        &short,
                        &tf_name,
                        &Rect::new(px + 10.0, py + 6.0, px + pw - 10.0, py + 6.0 + lh),
                        &name_ink,
                    );
                    ctx.draw_text(
                        &detail,
                        &tf_val,
                        &Rect::new(
                            px + 10.0,
                            py + 6.0 + lh,
                            px + pw - 10.0,
                            py + 6.0 + lh * 2.0,
                        ),
                        &body_ink,
                    );
                    ctx.draw_text(
                        &rank,
                        &tf_val,
                        &Rect::new(
                            px + 10.0,
                            py + 6.0 + lh * 2.0,
                            px + pw - 10.0,
                            py + 6.0 + lh * 3.0,
                        ),
                        &ink,
                    );
                }
                Ok(())
            }),
        ))
}

/// Ring geometry inside the `w×h` canvas — the ring occupies the left
/// h×h square so the right band is free for the hover bubble. Shared by the
/// draw pass and the pointer hit-test so they can never disagree.
fn donut_geom(w: f64, h: f64) -> (f64, f64, f64, f64) {
    let side = w.min(h);
    let cx = side * 0.5;
    let cy = h * 0.5;
    let r_out = (side * 0.5 - 4.0).max(1.0);
    let r_in = r_out * 0.62;
    (cx, cy, r_out, r_in)
}

/// Ring hit-test: pointer-local (x,y) in the `w×h` donut box — outside the
/// ring or in the hole → `None`; inside a slice's angular span → its index.
/// Zero angle is 12 o'clock going clockwise, matching the draw pass.
fn donut_hit(x: f64, y: f64, w: f64, h: f64, vals: &[f64]) -> Option<usize> {
    let (cx, cy, r_out, r_in) = donut_geom(w, h);
    let (dx, dy) = (x - cx, y - cy);
    let dist = (dx * dx + dy * dy).sqrt();
    let total: f64 = vals.iter().sum();
    if dist < r_in || dist > r_out || total <= 0.0 {
        return None;
    }
    let a = (dy.atan2(dx) + std::f64::consts::FRAC_PI_2).rem_euclid(std::f64::consts::TAU);
    let mut acc = 0.0;
    for (i, v) in vals.iter().enumerate() {
        acc += (v / total).max(0.0) * std::f64::consts::TAU;
        if a < acc {
            return Some(i);
        }
    }
    // Float tail: the last sliver owns the rounding remainder.
    vals.len()
        .checked_sub(1)
        .filter(|_| a < std::f64::consts::TAU)
}

/// One share column: title + donut + compact legend (`■ name … 42.0% · $x`).
/// `spec` bundles slices/hole label/formatter/hover wiring.
/// Zero-total data renders an honest empty note.
pub fn donut_cell(
    theme: &Theme,
    title: String,
    spec: DonutSpec<'_>,
    ctx: &mut ViewContext<Shell>,
) -> View {
    let DonutSpec {
        slices,
        center,
        fmt_v,
        key,
        handle,
        defer,
    } = spec;
    let total: f64 = slices.iter().map(|s| s.1).sum();
    let body: View = if total <= 0.0 {
        TextBlock::new()
            .text(t!("暂无数据"))
            .font_size(theme.label_size)
            .foreground(theme.subtle)
            .height(132.0)
            .into()
    } else {
        let mut legend: Vec<View> = Vec::with_capacity(slices.len());
        for (i, (name, v)) in slices.iter().enumerate() {
            let pct = format!("{:.1}", v / total * 100.0);
            let name_short: String = if name.chars().count() > 16 {
                crate::tf!("{}…", name.chars().take(15).collect::<String>())
            } else {
                name.clone()
            };
            let swatch: View = Border::new()
                .width(9.0)
                .height(9.0)
                .corner_radius(CornerRadius::uniform(2.0))
                .background(slice_brush(theme, i))
                .vertical_alignment(VerticalAlignment::Center)
                .into();
            legend.push(
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(6.0)
                    .children([
                        swatch,
                        TextBlock::new()
                            .text(name_short)
                            .font_size(theme.label_size)
                            .vertical_alignment(VerticalAlignment::Center)
                            .into(),
                        TextBlock::new()
                            .text(format!("{pct}% · {}", fmt_v(*v)))
                            .font_size(theme.label_size)
                            .foreground(theme.subtle)
                            .vertical_alignment(VerticalAlignment::Center)
                            .into(),
                    ]),
            );
        }
        StackPanel::new()
            .orientation(Orientation::Vertical)
            .spacing(6.0)
            .children([
                donut(
                    theme,
                    DonutSpec {
                        slices,
                        center,
                        fmt_v,
                        key,
                        handle,
                        defer,
                    },
                    ctx,
                ),
                StackPanel::new()
                    .orientation(Orientation::Vertical)
                    .spacing(3.0)
                    .keyed_children(crate::pages::keyed(legend)),
            ])
    };
    StackPanel::new()
        .orientation(Orientation::Vertical)
        .spacing(8.0)
        .children((
            TextBlock::new()
                .text(title)
                .font_size(theme.label_size)
                .foreground(theme.subtle),
            body,
        ))
}

/// Any row content with the list chrome every stacked list shares: 3 DIPs of
/// padding and a hairline underneath when the theme enables separators.
pub fn ruled_row(theme: &Theme, content: impl Into<View>) -> View {
    let divider: View = if theme.line_separators {
        Border::new().height(1.0).background(theme.divider).into()
    } else {
        Border::new().height(0.0).into()
    };
    Border::new().padding(Thickness::xy(0.0, 3.0)).content(
        StackPanel::new()
            .orientation(Orientation::Vertical)
            .spacing(0.0)
            .children((content.into(), divider)),
    )
}

#[cfg(test)]
mod tests {
    use super::donut_hit;

    // 200×132 donut box → ring parks in the left 132×132 square:
    // r_out=62, r_in≈38.4, center (66,66); x>132 is bubble territory.
    #[test]
    fn hit_resolves_ring_to_slice_index() {
        let vals = [50.0, 30.0, 20.0];
        // Right of center: slice 0 owns the first 50% of the circle.
        assert_eq!(donut_hit(120.0, 66.0, 200.0, 132.0, &vals), Some(0));
        // Just clockwise of 12 o'clock → slice 0; just counterclockwise →
        // last slice owns the rounding tail.
        assert_eq!(donut_hit(67.0, 8.0, 200.0, 132.0, &vals), Some(0));
        assert_eq!(donut_hit(65.0, 8.0, 200.0, 132.0, &vals), Some(2));
        // Bottom-left: slice 0 ended at 6 o'clock → slice 1.
        assert_eq!(donut_hit(30.0, 110.0, 200.0, 132.0, &vals), Some(1));
    }

    #[test]
    fn hit_rejects_hole_outside_and_empty() {
        let vals = [50.0, 50.0];
        assert_eq!(donut_hit(66.0, 66.0, 200.0, 132.0, &vals), None); // hole
        assert_eq!(donut_hit(190.0, 66.0, 200.0, 132.0, &vals), None); // right band
        assert_eq!(donut_hit(5.0, 5.0, 200.0, 132.0, &vals), None); // outside
        assert_eq!(donut_hit(120.0, 66.0, 200.0, 132.0, &[0.0, 0.0]), None); // no data
    }
}
