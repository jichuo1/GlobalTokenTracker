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
    let cols = cols.clamp(1, items.len().max(1));
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
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .children([
            cell(
                0,
                TextBlock::new()
                    .text(title)
                    .font_size(theme.title_size)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(theme.text)
                    .into(),
            ),
            cell(
                1,
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .spacing(10.0)
                    .keyed_children(keyed(actions)),
            ),
        ])
}

/// WinUI `CalendarDatePicker` reports the picked day as UTC-midnight
/// `DateTime` (100ns ticks since 1601); translate to the local day's
/// start-of-day epoch ms so the window follows local calendar dates.
fn picked_day_ms(d: Option<windows_time::DateTime>) -> Option<i64> {
    let ms = (d?.universal_time - 116_444_736_000_000_000) / 10_000;
    Some(globaltokentracker_core::viewmodel::utc_day_to_local_start(
        ms,
    ))
}

/// `自定义` range chrome: two calendar pickers (start day / last day) plus a
/// text echo of the resolved window — CalendarDatePicker has no `date`
/// setter in reactor 0.100, so the picked value is shown alongside.
fn custom_range_row(
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    start_ms: i64,
    end_ms: i64,
) -> View {
    let day_label = |v: &str| {
        TextBlock::new()
            .text(v)
            .font_size(theme.body_size)
            .foreground(theme.subtle)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
    };
    // end_ms is exclusive — echo the last INCLUDED day.
    let echo = format!(
        "{} → {}",
        fmt::day(Some(start_ms)),
        fmt::day(Some(end_ms - 1))
    );
    let from: View = CalendarDatePicker::new()
        .placeholder_text(tr("起始日期"))
        .on_date_changed(
            ctx.callback(|d: Option<windows_time::DateTime>| match picked_day_ms(d) {
                Some(ms) => Msg::SetCustomStart(ms),
                None => Msg::Noop,
            }),
        )
        .into();
    let to: View = CalendarDatePicker::new()
        .placeholder_text(tr("截止日期"))
        .on_date_changed(
            ctx.callback(|d: Option<windows_time::DateTime>| match picked_day_ms(d) {
                Some(ms) => Msg::SetCustomEnd(ms),
                None => Msg::Noop,
            }),
        )
        .into();
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(10.0)
        .children([
            day_label(tr("从")),
            from,
            day_label(tr("至")),
            to,
            day_label(&echo),
        ])
}

fn page_frame(theme: &Theme, body: View) -> View {
    let mut frame = Border::new().padding(Thickness::xy(24.0, 16.0));
    if let Some(bg) = &theme.page_bg {
        frame = frame.background(*bg);
    }
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
    let switch = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .vertical_alignment(VerticalAlignment::Center)
        .children((
            TextBlock::new()
                .text(t!("折线趋势"))
                .font_size(theme.label_size)
                .foreground(theme.subtle)
                .vertical_alignment(VerticalAlignment::Center),
            ToggleSwitch::new()
                .is_on(on)
                .on_toggled(ctx.callback(Msg::SetTrendLine)),
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
            args.canvas_ready < 6,
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
                a.input_tokens + a.output_tokens + a.cache_read_tokens
            };
            let columns = [
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
                        // Trend takes stage 1, donut `i` stage 2+i.
                        defer: args.canvas_ready < 2 + i,
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
        "apps" => {
            let tokens = |a: &globaltokentracker_core::store::AppSummary| {
                a.input_tokens + a.output_tokens + a.cache_read_tokens + a.cache_write_tokens
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
                    .spacing(8.0)
                    .children((
                        w::section_header(theme, Symbol::List, &tf!("{rl} · 按工具", rl)),
                        vstack(2.0, rows),
                    )),
            ))
        }
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
                    .spacing(8.0)
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
    refresh_w: f64,
    tools_x: f64,
    models_x: f64,
    refresh_x: f64,
}

fn chrome_geom(theme: &Theme) -> ChromeGeom {
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
    let refresh_x = models_x + pill_w + PICKER_GAP + label_w + GROUP_GAP;
    ChromeGeom {
        label_w,
        tools_w: pill_w,
        models_w: pill_w,
        refresh_w,
        tools_x,
        models_x,
        refresh_x,
    }
}

/// Filter-strip state bundle — keeps `filter_chrome`/`dropdown_overlay`
/// signatures tidy as more pickers join.
pub struct ChromeState<'a> {
    pub apps: &'a Option<Vec<String>>,
    pub models: &'a Option<Vec<String>>,
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
    let g = chrome_geom(theme);
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(PICKER_GAP)
        .margin(Thickness::xy(CHROME_LEFT, 4.0))
        .children((
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
            picker_button(
                theme,
                tr(refresh_label(state.refresh_secs)).to_string(),
                (g.label_w, g.refresh_w),
                state.open == Some(MenuKind::Refresh),
                MenuKind::Refresh,
                ctx,
            ),
        ))
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
    let mut col: Vec<View> = Vec::with_capacity(s.vm.apps.len() + 2);
    for name in &s.vm.apps {
        let checked = filter.as_ref().is_none_or(|f| f.contains(name));
        let n = name.clone();
        col.push(
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
                    .on_click(ctx.callback(|_| Msg::SetApps(None)))
                    .content(t!("全选")),
                Button::new()
                    .on_click(ctx.callback(|_| Msg::SetApps(Some(Vec::new()))))
                    .content(t!("清空")),
            )),
    );
    col
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

/// Refresh-cadence radio rows — single-select, so a pick light-dismisses the
/// panel (`SetRefreshSecs` clears `open_menu`). `0` seconds = file-watch only.
fn refresh_menu_items(theme: &Theme, secs: u64, ctx: &mut ViewContext<Shell>) -> Vec<View> {
    let mut rows: Vec<View> = Vec::with_capacity(REFRESH_OPTIONS.len());
    for (s, label) in REFRESH_OPTIONS {
        rows.push(
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
    let g = chrome_geom(theme);
    let (x, items) = match open {
        MenuKind::Tools => (g.tools_x, tools_menu_items(s, theme, state.apps, ctx)),
        MenuKind::Models => (g.models_x, models_menu_items(s, theme, state.models, ctx)),
        MenuKind::Refresh => (
            g.refresh_x,
            refresh_menu_items(theme, state.refresh_secs, ctx),
        ),
    };
    // Two-layer fill: `card_bg` is a *translucent* Fluent layer brush — over
    // page content it lets the rows beneath bleed through (reads as a broken
    // transparent popup). An opaque base under the tint composites to the
    // same elevated-card look the cards get over the window surface.
    let base = theme
        .page_bg
        .unwrap_or(Brush::Theme(ThemeBrush::SolidBackground));
    // Margin positions the card inside the content cell — plain Grid, no
    // Canvas (its empty-area hits can swallow presses meant for siblings).
    Border::new()
        .grid_row(2)
        .horizontal_alignment(HorizontalAlignment::Left)
        .vertical_alignment(VerticalAlignment::Top)
        .margin(Thickness::new(x, 4.0, 0.0, 0.0))
        .background(base)
        .border_brush(theme.card_border)
        .border_thickness(theme.card_border_thickness())
        .corner_radius(CornerRadius::uniform(theme.radius))
        .content(
            Border::new()
                .background(theme.card_bg)
                .corner_radius(CornerRadius::uniform(theme.radius - 1.0))
                .padding(Thickness::xy(12.0, 8.0))
                .content(
                    StackPanel::new()
                        .orientation(Orientation::Vertical)
                        .spacing(10.0)
                        .keyed_children(keyed(items)),
                ),
        )
}

/// Bundle for the overview page — keeps the signature under the arg limit
/// and makes the shell→page handoff explicit.
pub struct OverviewArgs<'a> {
    pub scanning: bool,
    pub config: &'a UiConfig,
    pub editing: bool,
    pub trend: &'a w::TrendHandle,
    pub heat: &'a w::HeatHandle,
    /// Reflow column count from the width ruler (stats + share grids).
    pub cols: usize,
    /// Bound to the 1-DIP ruler panel mounted on this page.
    pub ruler: &'a ElementRef<SwapChainPanel>,
    /// Hover state per share-donut column (pointer → Msg → invalidation).
    pub donuts: &'a [w::DonutHandle; 4],
    /// How many of the page's D2D charts may be mounted yet (trend = 1st,
    /// then one donut per stage). Held at 0 while a slide starts, then
    /// stepped up one per ~16ms from the slide's back half
    /// (`Msg::CanvasStage`); the rest of the time it sits at `usize::MAX`.
    /// Anything not yet allowed renders as a same-size placeholder
    /// (`widgets::defer_slot`).
    pub canvas_ready: usize,
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

    let mut col: Vec<View> = vec![header(
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
                Border::new().width(0.0).into()
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
    )];

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
                    let cols = fit_cols(width, REFLOW_MIN_CELL, 12.0, 4);
                    if cols != last.get() {
                        last.set(cols);
                        let _ = on_width.call(cols);
                    }
                }
            });
            Some(Box::new(move || drop(obs)))
        }
    });
    col.push(
        SwapChainPanel::new()
            .element_ref(args.ruler)
            .height(1.0)
            .into(),
    );

    if let Range::Custom { start_ms, end_ms } = s.vm.range {
        col.push(custom_range_row(theme, ctx, start_ms, end_ms));
    }

    for id in order.iter() {
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
/// cache, cost, duration — as the window narrows the (subtle) cache column
/// goes first, then the duration, then the tool.
const DETAIL_PLANS: [Plan; 4] = [
    &[
        (0, Some(90.0)),
        (1, Some(96.0)),
        (2, None),
        (3, Some(88.0)),
        (4, Some(88.0)),
        (5, Some(88.0)),
        (6, Some(92.0)),
        (7, Some(64.0)),
    ],
    &[
        (0, Some(84.0)),
        (1, Some(88.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
        (7, Some(60.0)),
    ],
    &[
        (0, Some(84.0)),
        (1, Some(88.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
    ],
    &[
        (0, Some(84.0)),
        (2, None),
        (3, Some(80.0)),
        (4, Some(80.0)),
        (6, Some(88.0)),
    ],
];

fn detail_layout(theme: &Theme, width: f64) -> (Vec<GridLength>, [Option<i32>; 8]) {
    let k = font_scale(theme);
    plan_layout(pick_plan(&DETAIL_PLANS, width, k), k)
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
    let mut cost = r
        .cost_usd
        .map(fmt::usd)
        .or_else(|| r.credits.map(|c| format!("{}cr", fmt::trim_f(c, 1))))
        .unwrap_or_else(|| "—".into());
    match r.cost_source.as_deref() {
        Some("estimated") => cost.push_str(" ≈"),
        Some("provider_reported") => cost.push_str(" ↺"),
        _ => {}
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
    row.content(Grid::new().columns(cols).keyed_children(keyed(cells)))
        .tooltip(r.raw_ref.clone().unwrap_or_default())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
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
    let list: View = virtual_rows(theme, d.rows.clone(), d.rows.len(), table.width, event_row);
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
            vstack(0.0, vec![ruler, detail_header(theme, table.width), list]),
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
        let header_btn = Button::new()
            .automation_name(tf!("配额组 {}", g.display))
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .on_click(ctx.callback(move |_| Msg::ToggleQuotaGroup(app_key.clone())))
            .content(
                Grid::new()
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
                            w::badge(
                                theme,
                                format!("{}%", fmt::trim_f(worst, 1)),
                                if worst > 80.0 {
                                    w::BadgeTone::Danger
                                } else if worst > 50.0 {
                                    w::BadgeTone::Warn
                                } else {
                                    w::BadgeTone::Muted
                                },
                            ),
                        ),
                        cell(
                            2,
                            TextBlock::new()
                                .text(if open { "▴" } else { "▾" })
                                .font_size(theme.label_size)
                                .foreground(theme.subtle)
                                .vertical_alignment(VerticalAlignment::Center)
                                .into(),
                        ),
                    ]),
            );
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
    if list.is_empty() {
        list.push(
            TextBlock::new()
                .text(t!("暂无配额数据"))
                .foreground(theme.subtle)
                .into(),
        );
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
        let state = h
            .last_error
            .as_ref()
            .map(|e| tf!("⚠ {e}", e))
            .unwrap_or_else(|| t!("正常").into());
        let err = h.last_error.is_some();
        list.push(w::card(
            theme,
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .children([
                    cell(
                        0,
                        StackPanel::new()
                            .orientation(Orientation::Vertical)
                            .spacing(4.0)
                            .children((
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
                                    .foreground(theme.subtle),
                            )),
                    ),
                    cell(
                        1,
                        if err {
                            w::badge(theme, state, w::BadgeTone::Danger)
                        } else {
                            TextBlock::new()
                                .text(state)
                                .font_size(theme.label_size)
                                .foreground(theme.ok)
                                .into()
                        },
                    ),
                ]),
        ));
    }
    if list.is_empty() {
        list.push(
            TextBlock::new()
                .text(t!("尚未扫描"))
                .foreground(theme.subtle)
                .into(),
        );
    }
    std::iter::once(header(theme, t!("数据源"), vec![]))
        .chain(list)
        .collect()
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

fn price_layout(theme: &Theme, width: f64) -> (Vec<GridLength>, [Option<i32>; 6]) {
    let k = font_scale(theme);
    plan_layout(pick_plan(&PRICE_PLANS, width, k), k)
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
    let search_box: View = TextBox::new()
        .text(search.query.to_string())
        .placeholder_text(t!("搜索模型，如 opus 4.6 或 gpt mini"))
        .on_text_changed(ctx.callback(Msg::PriceQuery))
        .width(380.0)
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
    let mut summary = if filtering {
        tf!("匹配 {} / {} 个模型 · {}", shown, total, sync)
    } else {
        tf!("{} 个模型 · {}", total, sync)
    };
    if let Some(e) = refresh.error {
        summary = format!("{summary} · {}", tf!("上次刷新失败：{}", e));
    }
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
    let controls: View = vstack(
        8.0,
        vec![
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(16.0)
                .children((search_box, only_disputed)),
            TextBlock::new()
                .text(summary)
                .font_size(theme.body_size)
                .foreground(theme.subtle)
                .into(),
        ],
    );

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
        .children([
            cell(
                0,
                StackPanel::new()
                    .spacing(2.0)
                    .vertical_alignment(VerticalAlignment::Center)
                    .keyed_children(keyed(left)),
            ),
            cell(
                1,
                StackPanel::new()
                    .orientation(Orientation::Horizontal)
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .vertical_alignment(VerticalAlignment::Center)
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

/// Toggle whose On/Off captions follow the UI language — the control's own
/// captions come from the OS locale, which left "开"/"关" next to English rows.
fn switch(theme: &Theme, is_on: bool, toggled: Callback<bool>) -> View {
    let caption = |text: &str| TextBlock::new().text(text).font_size(theme.body_size);
    ToggleSwitch::new().is_on(is_on).on_toggled(toggled).slots([
        SlotView::new(ToggleSwitchSlot::OnContent, caption(t!("开"))),
        SlotView::new(ToggleSwitchSlot::OffContent, caption(t!("关"))),
    ])
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
    let mut left: Vec<View> = vec![
        SymbolIcon::new().symbol(Symbol::Download).into(),
        TextBlock::new()
            .text(tf!("发现新版本 {}", rel.tag))
            .font_size(theme.body_size)
            .font_weight(FontWeight::SEMI_BOLD)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
    ];
    if let Some(e) = error.filter(|_| matches!(state, UpdateState::Available(_))) {
        left.push(
            TextBlock::new()
                .text(tf!("更新失败：{}", short_error(e)))
                .font_size(theme.label_size)
                .foreground(theme.danger)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    Border::new()
        .margin(Thickness::new(24.0, 8.0, 24.0, 0.0))
        .padding(Thickness::xy(theme.pad, 6.0))
        .background(theme.card_bg)
        .border_brush(theme.card_border)
        .border_thickness(theme.card_border_thickness())
        .corner_radius(CornerRadius::uniform(theme.radius))
        .content(
            Grid::new()
                .columns([GridLength::STAR, GridLength::Auto])
                .children([
                    cell(
                        0,
                        StackPanel::new()
                            .orientation(Orientation::Horizontal)
                            .spacing(8.0)
                            .vertical_alignment(VerticalAlignment::Center)
                            .keyed_children(keyed(left)),
                    ),
                    cell(
                        1,
                        StackPanel::new()
                            .orientation(Orientation::Horizontal)
                            .spacing(8.0)
                            .keyed_children(keyed(actions)),
                    ),
                ]),
        )
}

pub fn settings_page(
    config: &UiConfig,
    update: &UpdateState,
    update_error: Option<&str>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
) -> Vec<View> {
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

    vec![
        header(theme, tr("设置"), vec![]),
        w::section_header(theme, Symbol::FontColor, tr("外观")),
        w::card(
            theme,
            vstack(theme.gap, vec![theme_row, accent_row, font_row, scale_row]),
        ),
        w::section_header(theme, Symbol::Setting, tr("通用")),
        w::card(
            theme,
            vstack(theme.gap, vec![lang_row, autostart_row, close_row]),
        ),
        w::section_header(theme, Symbol::Sync, tr("更新")),
        w::card(
            theme,
            vstack(
                theme.gap,
                vec![version_row, channel_row, auto_row, status_row],
            ),
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
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 900.0, 1.0)),
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        // Cache goes first, then duration, then the tool column.
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 700.0, 1.0)),
            [0, 1, 2, 3, 4, 6, 7]
        );
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 600.0, 1.0)),
            [0, 1, 2, 3, 4, 6]
        );
        assert_eq!(kept(pick_plan(&DETAIL_PLANS, 500.0, 1.0)), [0, 2, 3, 4, 6]);
        // Nothing fits → the narrowest plan, never an empty layout.
        assert_eq!(kept(pick_plan(&DETAIL_PLANS, 200.0, 1.0)), [0, 2, 3, 4, 6]);
        // A larger body font makes the fixed columns wider: the same 640-wide
        // row that keeps 7 columns at 12pt drops to 5 at 17pt (k ≈ 1.42).
        assert_eq!(
            kept(pick_plan(&DETAIL_PLANS, 660.0, 1.0)),
            [0, 1, 2, 3, 4, 6, 7]
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
        let (cols, at) = plan_layout::<8>(DETAIL_PLANS[1], 1.0);
        assert_eq!(cols.len(), 7);
        // Cache (logical 5) is hidden; cost/duration slide left by one.
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
                Some(6)
            ]
        );
        // Pixel widths scale with the font; the star column stays a star.
        let (wide, _) = plan_layout::<8>(DETAIL_PLANS[1], 2.0);
        assert!(matches!(wide[0], GridLength::Pixel(w) if (w - 168.0).abs() < 1e-9));
        assert!(wide[2] == GridLength::STAR);
    }
}
