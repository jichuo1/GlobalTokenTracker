use super::*;
use globaltokentracker_core::store::ShareRow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpeedMetric {
    #[default]
    Output,
    Total,
    Input,
    Prefill,
    Decode,
}
impl SpeedMetric {
    const ALL: [Self; 5] = [
        Self::Output,
        Self::Total,
        Self::Input,
        Self::Prefill,
        Self::Decode,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Output => "输出",
            Self::Total => "总吞吐",
            Self::Input => "输入",
            Self::Prefill => "预填",
            Self::Decode => "解码",
        }
    }
    fn value(self, r: &ShareRow) -> Option<f64> {
        match self {
            Self::Output => r.output_tps(),
            Self::Total => r.total_tps(),
            Self::Input => r.input_tps(),
            Self::Prefill => r.prefill_tps(),
            Self::Decode => r.decode_tps(),
        }
    }
}

pub struct SpeedArgs<'a> {
    pub by_app: bool,
    pub metric: SpeedMetric,
    pub page: usize,
    pub cols: usize,
    pub cal: CalView,
    pub clock_dial: &'a w::ClockDialHandle,
}

fn ranked(rows: &[ShareRow], metric: SpeedMetric) -> Vec<&ShareRow> {
    let mut ranked: Vec<_> = rows.iter().filter(|r| metric.value(r).is_some()).collect();
    ranked.sort_by(|a, b| {
        metric
            .value(b)
            .unwrap_or(0.0)
            .total_cmp(&metric.value(a).unwrap_or(0.0))
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked
}

fn metric_cell(theme: &Theme, label: &str, value: Option<f64>, estimated: bool) -> View {
    vstack(
        4.0,
        vec![
            TextBlock::new()
                .text(tr(label))
                .foreground(theme.subtle)
                .font_size(theme.label_size)
                .into(),
            TextBlock::new()
                .text(value.map_or_else(
                    || "—".into(),
                    |v| format!("{}{}", if estimated { "≈" } else { "" }, fmt::speed(v)),
                ))
                .foreground(theme.text)
                .font_size(theme.body_size)
                .font_weight(FontWeight::SEMI_BOLD)
                .into(),
        ],
    )
}

pub fn speed_page(
    snap: Option<&Snapshot>,
    theme: &Theme,
    ctx: &mut ViewContext<Shell>,
    args: &SpeedArgs,
) -> Vec<View> {
    let Some(s) = snap else {
        return vec![loading(theme, true)];
    };
    let rows = if args.by_app {
        &s.vm.by_app_speed
    } else {
        &s.vm.by_model
    };
    let ranking = ranked(rows, args.metric);
    let range_items: Vec<KeyedView> = Range::LIST
        .iter()
        .map(|r| {
            KeyedView::new(
                r.label(),
                SelectorBarItem::new()
                    .text(tr(r.label()))
                    .is_selected(s.vm.range == *r),
            )
        })
        .chain(std::iter::once(KeyedView::new(
            "custom",
            SelectorBarItem::new()
                .text(t!("自定义"))
                .is_selected(matches!(s.vm.range, Range::Custom { .. })),
        )))
        .collect();
    let range = SelectorBar::new()
        .on_selected_text_changed(ctx.callback(|v: Option<String>| {
            let v = v.unwrap_or_default();
            if v == tr("自定义") {
                return Msg::PickCustomRange;
            }
            Msg::SetRange(
                Range::LIST
                    .iter()
                    .find(|r| tr(r.label()) == v)
                    .copied()
                    .unwrap_or_default(),
            )
        }))
        .collection_slot(SelectorBarSlot::Items, range_items);
    let mut blocks = vec![header(
        theme,
        t!("速度"),
        vec![
            range,
            Button::new()
                .on_click(ctx.callback(|_| Msg::Rescan))
                .content(t!("刷新")),
        ],
    )];
    if let Range::Custom { start_ms, end_ms } = s.vm.range {
        blocks.push(custom_range_row(
            theme,
            ctx,
            start_ms,
            end_ms,
            args.cal,
            args.cols,
            args.clock_dial,
        ));
    }
    let (mut output, mut timed, mut estimated) = (0u64, 0u64, 0u64);
    for r in rows {
        output += r.speed_out;
        timed += r.speed_ms;
        estimated += r.speed_est_ms;
    }
    let overall = (timed > 0).then(|| output as f64 / timed as f64 * 1000.0);
    let name = |r: &ShareRow| {
        if args.by_app {
            app_display(&r.name).to_string()
        } else {
            r.name.clone()
        }
    };
    let fastest = ranking
        .first()
        .map(|r| name(r))
        .unwrap_or_else(|| "—".into());
    let summary = vec![
        w::stat_card(
            theme,
            Symbol::Go,
            t!("加权输出速度"),
            overall.map_or_else(|| "—".into(), fmt::speed),
            t!("按有效计时加权").into(),
            true,
            None,
        ),
        w::stat_card(
            theme,
            Symbol::Favorite,
            t!("当前榜首"),
            ranking
                .first()
                .and_then(|r| args.metric.value(r))
                .map_or_else(|| "—".into(), fmt::speed),
            fastest,
            false,
            None,
        ),
        w::stat_card(
            theme,
            Symbol::Clock,
            t!("估算计时占比"),
            if timed > 0 {
                format!("{:.1}%", estimated as f64 / timed as f64 * 100.0)
            } else {
                "—".into()
            },
            tf!("{} 项参与排名", ranking.len()),
            false,
            None,
        ),
    ];
    blocks.push(reflow_grid(theme, summary, args.cols.min(3)));
    let groups = SelectorBar::new()
        .on_selected_text_changed(ctx.callback(|value: Option<String>| {
            Msg::SpeedGroup(value.as_deref() == Some(tr("按工具")))
        }))
        .collection_slot(
            SelectorBarSlot::Items,
            [
                KeyedView::new(
                    "model",
                    SelectorBarItem::new()
                        .text(t!("按模型"))
                        .is_selected(!args.by_app),
                ),
                KeyedView::new(
                    "app",
                    SelectorBarItem::new()
                        .text(t!("按工具"))
                        .is_selected(args.by_app),
                ),
            ],
        );
    let metrics = SelectorBar::new()
        .on_selected_text_changed(ctx.callback(|value: Option<String>| {
            Msg::SpeedMetric(
                SpeedMetric::ALL
                    .into_iter()
                    .find(|m| Some(tr(m.label())) == value.as_deref())
                    .unwrap_or_default(),
            )
        }))
        .collection_slot(
            SelectorBarSlot::Items,
            SpeedMetric::ALL
                .into_iter()
                .map(|metric| {
                    KeyedView::new(
                        metric.label(),
                        SelectorBarItem::new()
                            .text(tr(metric.label()))
                            .is_selected(metric == args.metric),
                    )
                })
                .collect::<Vec<_>>(),
        );
    blocks.push(w::card(theme,vstack(10.0,vec![groups,metrics,
        TextBlock::new().text(t!("按有效计时加权；缺少计时的记录不参与速度排名。请求数为筛选范围内的总请求数。")).font_size(theme.label_size).foreground(theme.subtle).text_wrapping(TextWrapping::Wrap).into(),
        TextBlock::new().text(t!("每页 25 项；≈ 表示包含时间戳估算，未报告的分项显示 —。")).font_size(theme.label_size).foreground(theme.subtle).text_wrapping(TextWrapping::Wrap).into(),
    ])));
    let peak = ranking
        .first()
        .and_then(|r| args.metric.value(r))
        .filter(|value| *value > 0.0)
        .unwrap_or(1.0);
    let page = args.page.min(ranking.len().saturating_sub(1) / 25);
    let start = page * 25;
    if !ranking.is_empty() {
        blocks.push(header(
            theme,
            &tf!(
                "第 {}–{} 项 / 共 {} 项",
                start + 1,
                (start + 25).min(ranking.len()),
                ranking.len()
            ),
            vec![
                Button::new()
                    .is_enabled(page > 0)
                    .on_click(ctx.callback(move |_| Msg::SpeedPage(page.saturating_sub(1))))
                    .content(t!("上一页")),
                Button::new()
                    .is_enabled(start + 25 < ranking.len())
                    .on_click(ctx.callback(move |_| Msg::SpeedPage(page + 1)))
                    .content(t!("下一页")),
            ],
        ));
    }
    let mut items = Vec::new();
    for (index, r) in ranking.iter().enumerate().skip(start).take(25) {
        let approx = r.speed_est_ms > 0 && !matches!(args.metric, SpeedMetric::Prefill);
        let value = args.metric.value(r).unwrap_or(0.0);
        let header = Grid::new()
            .columns([GridLength::Pixel(40.0), GridLength::STAR, GridLength::Auto])
            .column_spacing(12.0)
            .keyed_children([
                KeyedView::new(
                    "rank",
                    cell(
                        0,
                        TextBlock::new()
                            .text(format!("{:02}", index + 1))
                            .font_size(theme.h2_size)
                            .foreground(if index < 3 {
                                theme.accent
                            } else {
                                theme.subtle
                            })
                            .into(),
                    ),
                ),
                KeyedView::new(
                    "name",
                    cell(
                        1,
                        TextBlock::new()
                            .text(name(r))
                            .font_size(theme.h2_size)
                            .font_weight(FontWeight::SEMI_BOLD)
                            .text_trimming(TextTrimming::CharacterEllipsis)
                            .into(),
                    ),
                ),
                KeyedView::new(
                    "value",
                    cell(
                        2,
                        TextBlock::new()
                            .text(format!(
                                "{}{}",
                                if approx { "≈" } else { "" },
                                fmt::speed(value)
                            ))
                            .font_size(theme.h2_size + 4.0)
                            .font_weight(FontWeight::SEMI_BOLD)
                            .foreground(theme.accent)
                            .into(),
                    ),
                ),
            ]);
        let details = SpeedMetric::ALL
            .into_iter()
            .filter(|m| *m != args.metric)
            .map(|m| {
                metric_cell(
                    theme,
                    m.label(),
                    m.value(r),
                    r.speed_est_ms > 0 && m != SpeedMetric::Prefill,
                )
            })
            .collect();
        let detail_grid = reflow_grid(theme, details, args.cols);
        let ratio = r.speed_est_ratio() * 100.0;
        items.push(
            w::card(
                theme,
                vstack(
                    12.0,
                    vec![
                        header,
                        ProgressBar::new()
                            .minimum(0.0)
                            .maximum(100.0)
                            .value(value / peak * 100.0)
                            .height(4.0)
                            .into(),
                        detail_grid,
                        TextBlock::new()
                            .text(tf!(
                                "{} 次请求 · 有效计时 {} 秒 · 估算计时 {}%",
                                fmt::tokens_exact(r.events),
                                fmt::trim_f(r.speed_ms as f64 / 1000.0, 1),
                                fmt::trim_f(ratio, 1)
                            ))
                            .font_size(theme.label_size)
                            .foreground(theme.subtle)
                            .text_wrapping(TextWrapping::Wrap)
                            .into(),
                    ],
                ),
            )
            .tooltip(format!("{}\n{}", name(r), speed_tip(r))),
        );
    }
    if items.is_empty() {
        items.push(w::card(
            theme,
            TextBlock::new()
                .text(t!("当前筛选下没有可用于此项排名的计时数据。"))
                .foreground(theme.subtle)
                .font_size(theme.body_size)
                .into(),
        ));
    }
    blocks.push(vstack(theme.gap, items));
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranking_uses_weighted_rates_and_keeps_missing_metrics_out() {
        let rows = vec![
            ShareRow {
                name: "a".into(),
                events: 100,
                speed_out: 100,
                speed_ms: 1000,
                ..Default::default()
            },
            ShareRow {
                name: "b".into(),
                events: 1,
                speed_out: 20,
                speed_ms: 100,
                ..Default::default()
            },
            ShareRow {
                name: "unknown".into(),
                events: 1000,
                ..Default::default()
            },
        ];
        assert_eq!(
            ranked(&rows, SpeedMetric::Output)
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(ranked(&rows, SpeedMetric::Prefill).len(), 0);
    }
}
