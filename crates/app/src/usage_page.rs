//! 用量页，照 magpie 的用量页排：时间段和总花费、四个统计块、按时间的柱状图（「全部」可以换成最近 53 周的热力图），
//! 按供应商 / agent / 模型的列表。
//! 数据在 `Store` 里（`usage.rs`），这里只管画和选时间段。

use std::borrow::Cow;

use chrono::{Datelike as _, Duration, Local, NaiveDate, TimeZone as _};
use confdeck_core::agent::AgentKind;
use confdeck_core::balance::{self, BalanceError, Currency};
use confdeck_core::preset;
use confdeck_core::quota::{self, Quota, QuotaError, Span};
use confdeck_core::usage::Tokens;
use confdeck_core::usage::summary::{
    HEATMAP_WEEKS, Heatmap, Period, Source, Step, Summary, Totals,
};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Store};
use crate::theme::Theme;
use crate::ui;
use crate::usage::{self, Loaded, PERIODS, QuotaOf};

/// 柱状图的高度。
const CHART_HEIGHT: f32 = 72.;
/// 列表里 token 数那一列的宽度：放得下「输入 1.2M · 输出 812K · 缓存 48M · 命中率 96%」和英文的同一句。
const NUM_WIDTH: f32 = 264.;

/// 下面那张列表按什么分。
#[derive(Clone, Copy, PartialEq)]
enum Group {
    Sources,
    Agents,
    Models,
}

const GROUPS: [Group; 3] = [Group::Sources, Group::Agents, Group::Models];

/// 「全部」的图表画成什么。
#[derive(Clone, Copy, PartialEq)]
enum View {
    Bars,
    Heat,
}

const VIEWS: [View; 2] = [View::Bars, View::Heat];

/// 热力图按什么分深浅。
#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Tokens,
    Requests,
    Cost,
}

const METRICS: [Metric; 3] = [Metric::Tokens, Metric::Requests, Metric::Cost];

pub struct UsagePage {
    store: Entity<Store>,
    period: Period,
    /// 上一个时间段和这次切换的编号，给分段标签的滑块放动画。
    period_from: Option<(usize, u64)>,
    group: Group,
    group_from: Option<(usize, u64)>,
    view: View,
    view_from: Option<(usize, u64)>,
    metric: Metric,
    metric_from: Option<(usize, u64)>,
    clicks: u64,
    _subscription: Subscription,
}

impl UsagePage {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let _subscription = cx.observe(&store, |_, _, cx| cx.notify());
        store.update(cx, |store, cx| store.refresh_usage(false, cx));
        Self {
            store,
            period: Period::Month,
            period_from: None,
            group: Group::Sources,
            group_from: None,
            view: View::Heat,
            view_from: None,
            metric: Metric::Tokens,
            metric_from: None,
            clicks: 0,
            _subscription,
        }
    }

    fn set_period(&mut self, period: Period, cx: &mut Context<Self>) {
        if period == self.period {
            return;
        }
        let from = PERIODS.iter().position(|p| *p == self.period).unwrap_or(0);
        self.clicks += 1;
        self.period_from = Some((from, self.clicks));
        self.period = period;
        cx.notify();
    }

    fn set_group(&mut self, group: Group, cx: &mut Context<Self>) {
        if group == self.group {
            return;
        }
        let from = GROUPS.iter().position(|g| *g == self.group).unwrap_or(0);
        self.clicks += 1;
        self.group_from = Some((from, self.clicks));
        self.group = group;
        cx.notify();
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if view == self.view {
            return;
        }
        let from = VIEWS.iter().position(|v| *v == self.view).unwrap_or(0);
        self.clicks += 1;
        self.view_from = Some((from, self.clicks));
        self.view = view;
        cx.notify();
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        if metric == self.metric {
            return;
        }
        let from = METRICS.iter().position(|m| *m == self.metric).unwrap_or(0);
        self.clicks += 1;
        self.metric_from = Some((from, self.clicks));
        self.metric = metric;
        cx.notify();
    }

    /// 「全部」的图表：柱状图或热力图，左上角切换。别的时间段只有柱状图。
    fn chart(
        &self,
        t: &Theme,
        summary: &Summary,
        map: &Heatmap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        if self.period != Period::All {
            return chart(t, summary, None);
        }
        let views: Vec<(View, SharedString)> = vec![
            (View::Bars, t!("usage.bars").into()),
            (View::Heat, t!("usage.heatmap").into()),
        ];
        let switch = div().id("view").bg(t.pill).rounded(px(8.)).child(ui::seg(
            t,
            &views,
            self.view,
            self.view_from,
            window,
            cx.listener(|this, view: &View, _, cx| this.set_view(*view, cx)),
        ));
        match self.view {
            View::Bars => chart(t, summary, Some(switch.into_any_element())),
            View::Heat => {
                let metrics: Vec<(Metric, SharedString)> = vec![
                    (Metric::Tokens, t!("usage.tokens").into()),
                    (Metric::Requests, t!("usage.requests").into()),
                    (Metric::Cost, t!("usage.cost").into()),
                ];
                let pick = div().id("metric").bg(t.pill).rounded(px(8.)).child(ui::seg(
                    t,
                    &metrics,
                    self.metric,
                    self.metric_from,
                    window,
                    cx.listener(|this, metric: &Metric, _, cx| this.set_metric(*metric, cx)),
                ));
                chart_card(t)
                    .pb(px(12.))
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .mb(px(10.))
                            .child(switch)
                            .child(div().flex_1())
                            .child(pick),
                    )
                    .child(heat_grid(t, map, self.metric, heat_cell(window)))
            }
        }
    }

    /// 列表上面的分段标签：供应商 / Agent / 模型，一次只列一组。
    fn group_head(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let groups: Vec<(Group, SharedString)> = vec![
            (Group::Sources, t!("usage.sources").into()),
            (Group::Agents, t!("usage.agents").into()),
            (Group::Models, t!("usage.models").into()),
        ];
        div().h_flex().mt(px(14.)).mb(px(8.)).child(
            div().id("group").bg(t.pill).rounded(px(8.)).child(ui::seg(
                t,
                &groups,
                self.group,
                self.group_from,
                window,
                cx.listener(|this, group: &Group, _, cx| this.set_group(*group, cx)),
            )),
        )
    }

    fn head(
        &self,
        t: &Theme,
        summary: Option<&Summary>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let periods: Vec<(Period, SharedString)> = PERIODS
            .iter()
            .map(|period| (*period, period_label(*period)))
            .collect();
        let cost = summary.and_then(|summary| {
            let total = &summary.total;
            let text = cost_text(total)?;
            let tip = (total.unpriced > 0).then(|| unpriced_text(total.unpriced));
            Some(
                div()
                    .id("usage-cost")
                    .h_flex()
                    .items_baseline()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.fg)
                            .child(format!("≈{text}")),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.muted)
                            .child(t!("usage.prices")),
                    )
                    .when_some(tip, |d, tip| d.tooltip(ui::tooltip(tip))),
            )
        });
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .h(px(34.))
            .mt(px(4.))
            .mb(px(6.))
            .child(div().id("period").bg(t.pill).rounded(px(8.)).child(ui::seg(
                t,
                &periods,
                self.period,
                self.period_from,
                window,
                cx.listener(|this, period: &Period, _, cx| this.set_period(*period, cx)),
            )))
            .child(div().flex_1())
            .children(cost)
    }
}

impl Render for UsagePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let store = self.store.read(cx);
        let loaded = store.usage.loaded.clone();
        let error = store.usage.error.clone();
        let summary = loaded.as_ref().map(|loaded| loaded.summary(self.period));
        let mut page = div().v_flex().child(self.head(&t, summary, window, cx));
        if let Some(error) = error {
            page = page.child(
                div()
                    .h_flex()
                    .items_start()
                    .gap(px(8.))
                    .mb(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(t.danger.opacity(0.1))
                    .text_size(px(12.))
                    .text_color(t.danger)
                    .child(ui::icon("circle-alert", 14., t.danger).mt(px(1.)))
                    .child(t!("usage.load_error", error = error)),
            );
        }
        let store = self.store.read(cx);
        page = page
            .children(quotas(&t, store, loaded.as_deref(), window, cx))
            .children(balances(&t, store, window, cx));
        let (Some(loaded), Some(summary)) = (loaded.as_ref(), summary) else {
            return page.child(skeleton(&t));
        };
        if summary.total.requests == 0 {
            return page.child(
                stats_card(&t).child(
                    div()
                        .flex_1()
                        .bg(t.card)
                        .rounded(px(11.))
                        .py(px(18.))
                        .px(px(16.))
                        .flex()
                        .justify_center()
                        .text_color(t.muted)
                        .child(empty_text(self.period)),
                ),
            );
        }
        let chart = self.chart(&t, summary, &loaded.heatmap, window, cx);
        let groups = self.group_head(&t, window, cx);
        let store = self.store.read(cx);
        page.child(stats(&t, &summary.total))
            .child(chart)
            .child(groups)
            .child(match self.group {
                Group::Sources => sources_list(&t, summary, loaded, store, window, cx),
                Group::Agents => agents_list(&t, summary, store, window, cx),
                Group::Models => models_list(&t, summary, window, cx),
            })
    }
}

/// 余额卡：能查余额的供应商一家一张，三张一排（magpie 的 `.subscription-usage`）。
fn balances(t: &Theme, store: &Store, window: &Window, cx: &App) -> Option<Div> {
    let cards: Vec<Div> = store
        .providers
        .iter()
        .filter(|provider| balance::supported(provider))
        .map(|provider| {
            let state = store.usage.balances.get(&provider.id);
            let (label, amount): (SharedString, AnyElement) = match state {
                None => (
                    t!("usage.balance").into(),
                    div()
                        .w(px(64.))
                        .h(px(16.))
                        .rounded(px(4.))
                        .bg(t.pill)
                        .into_any_element(),
                ),
                Some(Ok(found)) => match found.left {
                    Some(left) => (
                        t!("usage.balance").into(),
                        money_text(t, left, found.currency).into_any_element(),
                    ),
                    None => (
                        t!("usage.spent").into(),
                        money_text(t, found.used.unwrap_or(0.), found.currency).into_any_element(),
                    ),
                },
                Some(Err(BalanceError::Auth)) => (
                    t!("usage.balance").into(),
                    div()
                        .text_size(px(12.5))
                        .text_color(t.danger)
                        .child(t!("usage.balance_auth"))
                        .into_any_element(),
                ),
                Some(Err(err)) => {
                    let why: SharedString = match err {
                        BalanceError::Net(err) => crate::net::describe(err),
                        _ => t!("usage.balance_unknown").into(),
                    };
                    (
                        t!("usage.balance").into(),
                        div()
                            .id(ElementId::Name(
                                format!("balance-error-{}", provider.id).into(),
                            ))
                            .text_size(px(15.))
                            .text_color(t.faint)
                            .tooltip(ui::tooltip(why))
                            .child("—")
                            .into_any_element(),
                    )
                }
            };
            account_card(t)
                .child(card_head(
                    t,
                    ui::logo(data::logo(provider), 18., t, window, cx),
                    provider.name.clone(),
                    None,
                ))
                .child(
                    div()
                        .h_flex()
                        .h(px(20.))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .text_color(t.muted)
                                .child(label),
                        )
                        .child(amount),
                )
        })
        .collect();
    card_grid(cards, 3)
}

/// 余额、额度卡片的外框。
fn account_card(t: &Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .v_flex()
        .gap(px(12.))
        .pt(px(12.))
        .pb(px(11.))
        .px(px(14.))
        .bg(t.card)
        .border_1()
        .border_color(t.line)
        .rounded(px(12.))
}

/// 卡片顶上一行：图标、名字，右边是套餐名。
fn card_head(
    t: &Theme,
    logo: AnyElement,
    name: impl Into<SharedString>,
    plan: Option<&str>,
) -> Div {
    div()
        .h_flex()
        .gap(px(8.))
        .child(logo)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(name.into()),
        )
        .children(plan.map(|plan| {
            let mut plan = plan.to_string();
            if let Some(first) = plan.get(..1) {
                plan = first.to_uppercase() + &plan[1..];
            }
            div().text_size(px(11.5)).text_color(t.muted).child(plan)
        }))
}

/// 卡片 `per_row` 张一排。
fn card_grid(cards: Vec<Div>, per_row: usize) -> Option<Div> {
    if cards.is_empty() {
        return None;
    }
    let mut rows = div().v_flex().gap(px(8.)).mb(px(8.));
    let mut cards = cards.into_iter().peekable();
    while cards.peek().is_some() {
        let mut row = div().flex().flex_row().gap(px(8.));
        for slot in 0..per_row {
            row = match cards.next() {
                Some(card) => row.child(card),
                // 不满的那排补空位，卡片宽度和上一排一样。空位也要有卡片的内边距和边框：
                // `flex_1` 从 0 起分剩下的宽度，内边距和边框算在起点里，没有的话空位会窄一截。
                None if slot > 0 => row.child(
                    div()
                        .flex_1()
                        .px(px(14.))
                        .border_1()
                        .border_color(gpui_kit::transparent_black()),
                ),
                None => row,
            };
        }
        rows = rows.child(row);
    }
    Some(rows)
}

/// 额度卡：Claude、Codex、Grok Build 官方（装了的；没有官方登录的不画），再是能查套餐额度的供应商。
/// 照 magpie 的 `.subscription-card`：每个窗口一格，格里是窗口名和用了多少、进度条、多久后重置。
/// 卡片两张一排、窗口三格一排（最多三个窗口），同一排的卡片一样高，出错的卡不会空出一大块。
fn quotas(
    t: &Theme,
    store: &Store,
    loaded: Option<&Loaded>,
    window: &Window,
    cx: &App,
) -> Option<Div> {
    let mut cards = Vec::new();
    for agent in store.agents.iter().filter(|a| a.installed) {
        let state = match agent.kind {
            AgentKind::Codex => match loaded.and_then(|l| l.codex.as_ref()) {
                Some(found) => Some(Ok(found)),
                None => continue,
            },
            AgentKind::ClaudeCode | AgentKind::GrokBuild => {
                match store.usage.quotas.get(&QuotaOf::Agent(agent.kind)) {
                    Some(Err(QuotaError::NotLoggedIn)) => continue,
                    Some(result) => Some(result.as_ref()),
                    None => None,
                }
            }
            _ => continue,
        };
        let logo = ui::logo(agent.logo, 18., t, window, cx);
        cards.push(quota_card(t, agent.kind.id(), logo, agent.name, state));
    }
    for provider in store.providers.iter().filter(|p| quota::plan_supported(p)) {
        let state = store
            .usage
            .quotas
            .get(&QuotaOf::Provider(provider.id.clone()))
            .map(Result::as_ref);
        let logo = ui::logo(data::logo(provider), 18., t, window, cx);
        cards.push(quota_card(
            t,
            &provider.id,
            logo,
            provider.name.clone(),
            state,
        ));
    }
    card_grid(cards, 2)
}

fn quota_card(
    t: &Theme,
    id: &str,
    logo: AnyElement,
    name: impl Into<SharedString>,
    state: Option<Result<&Quota, &QuotaError>>,
) -> Div {
    let plan = match state {
        Some(Ok(found)) => found.plan.as_deref(),
        _ => None,
    };
    let card = account_card(t).child(card_head(t, logo, name, plan));
    let status = |text: SharedString, color: Hsla| {
        div()
            .h(px(20.))
            .text_size(px(12.5))
            .text_color(color)
            .child(text)
    };
    match state {
        // 还在查：两格灰条。
        None => card.child(window_cells(
            (0..2)
                .map(|_| {
                    div()
                        .v_flex()
                        .gap(px(6.))
                        .child(div().w(px(72.)).h(px(12.)).rounded(px(3.)).bg(t.pill))
                        .child(div().h(px(4.)).rounded(px(2.)).bg(t.pill))
                })
                .collect(),
        )),
        Some(Ok(found)) => {
            let now = Local::now().timestamp_millis();
            let cells = found
                .windows
                .iter()
                .enumerate()
                .map(|(i, w)| quota_window(t, &format!("{id}-{i}"), w, now))
                .collect();
            card.child(window_cells(cells))
        }
        Some(Err(QuotaError::Expired)) => {
            card.child(status(t!("usage.quota_expired").into(), t.danger))
        }
        Some(Err(QuotaError::Auth)) => {
            card.child(status(t!("usage.balance_auth").into(), t.danger))
        }
        Some(Err(QuotaError::NoPlan)) => card.child(status(t!("usage.quota_none").into(), t.muted)),
        Some(Err(err)) => {
            let why: SharedString = match err {
                QuotaError::Net(err) => crate::net::describe(err),
                _ => t!("usage.balance_unknown").into(),
            };
            card.child(
                div()
                    .id(ElementId::Name(format!("quota-error-{id}").into()))
                    .h(px(20.))
                    .text_size(px(15.))
                    .text_color(t.faint)
                    .tooltip(ui::tooltip(why))
                    .child("—"),
            )
        }
    }
}

/// 窗口三格一排，不满的补空位：各张卡的格子一样宽。
fn window_cells(cells: Vec<Div>) -> Div {
    let mut rows = div().v_flex().gap(px(10.));
    let mut cells = cells.into_iter().peekable();
    while cells.peek().is_some() {
        let mut row = div().flex().flex_row().gap(px(16.));
        for _ in 0..3 {
            row = row.child(match cells.next() {
                Some(cell) => cell.flex_1().min_w_0(),
                None => div().flex_1(),
            });
        }
        rows = rows.child(row);
    }
    rows
}

/// 一个窗口：名字和用了百分之几、进度条、多久后重置（悬停说几点）。
fn quota_window(t: &Theme, id: &str, w: &quota::Window, now: i64) -> Div {
    let used = w.used.max(0.);
    let full = used >= 100.;
    let reset = w.resets_at.filter(|at| *at > now);
    let cell = div()
        .id(ElementId::Name(format!("quota-{id}").into()))
        .v_flex()
        .child(
            div()
                .h_flex()
                .items_baseline()
                .gap(px(6.))
                .mb(px(5.))
                .text_size(px(11.5))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.muted)
                        .child(span_label(&w.span)),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if full { t.danger } else { t.fg })
                        .child(format!("{used:.0}%")),
                ),
        )
        .child(
            div().h(px(4.)).rounded(px(2.)).bg(t.pill).child(
                div()
                    .h_full()
                    .w(relative((used / 100.).min(1.) as f32))
                    .rounded(px(2.))
                    .bg(if full { t.danger } else { t.accent }),
            ),
        )
        .child(
            div()
                .h(px(14.))
                .mt(px(4.))
                .text_size(px(10.5))
                .text_color(t.faint)
                .children(reset.map(|at| until_text(at - now))),
        );
    // 格子里只写百分比（「每周 · Opus」和「已用 12%」并排放不下），悬停说全。
    let mut tip = t!("usage.quota_used", percent = format!("{used:.0}")).to_string();
    if let Some(at) = reset.and_then(|at| Local.timestamp_millis_opt(at).earliest()) {
        let when = if rust_i18n::locale().starts_with("zh") {
            at.format("%-m月%-d日 %H:%M").to_string()
        } else {
            at.format("%b %-d, %H:%M").to_string()
        };
        tip.push_str(" · ");
        tip.push_str(&t!("usage.quota_resets_at", when = when));
    }
    div().child(cell.tooltip(ui::tooltip(tip)))
}

fn span_label(span: &Span) -> String {
    match span {
        Span::FiveHours => t!("usage.span_5h").to_string(),
        Span::Week => t!("usage.span_week").to_string(),
        Span::Month => t!("usage.span_month").to_string(),
        Span::WeekOf(model) => format!("{} · {model}", t!("usage.span_week")),
        Span::Minutes(minutes) if minutes % 1440 == 0 => {
            t!("usage.span_days", count = minutes / 1440).to_string()
        }
        Span::Minutes(minutes) => t!("usage.span_hours", count = minutes.div_ceil(60)).to_string(),
        Span::Period => t!("usage.span_period").to_string(),
    }
}

/// 「3 小时后重置」：一小时内按分钟，两天内按小时，再长按天。
fn until_text(millis: i64) -> String {
    let minutes = (millis / 60_000).max(1);
    let (count, key) = if minutes < 60 {
        (minutes, "usage.resets_minutes")
    } else if minutes < 48 * 60 {
        (minutes / 60, "usage.resets_hours")
    } else {
        (minutes / 1440, "usage.resets_days")
    };
    t!(key, count = count).to_string()
}

fn money_text(t: &Theme, amount: f64, currency: Currency) -> Div {
    let sign = match currency {
        Currency::Cny => "¥",
        Currency::Usd => "$",
    };
    div()
        .text_size(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if amount <= 0. { t.danger } else { t.fg })
        .child(format!("{sign}{amount:.2}"))
}

fn period_label(period: Period) -> SharedString {
    match period {
        Period::Today => t!("usage.today"),
        Period::Week => t!("usage.week"),
        Period::Month => t!("usage.month"),
        Period::All => t!("usage.all"),
    }
    .into()
}

fn empty_text(period: Period) -> SharedString {
    match period {
        Period::Today => t!("usage.none_today"),
        Period::Week => t!("usage.none_week"),
        Period::Month => t!("usage.none_month"),
        Period::All => t!("usage.none_all"),
    }
    .into()
}

/// 统计块的外框：线色的底、1px 的缝，里面的块各自是卡片色（magpie 的 `.stats`）。
fn stats_card(t: &Theme) -> Div {
    // 不用 `h_flex`：它让子元素垂直居中，没有小字的那块就矮一截。
    div()
        .flex()
        .flex_row()
        .flex_none()
        .gap(px(1.))
        .bg(t.line)
        .border_1()
        .border_color(t.line)
        .rounded(px(12.))
}

fn stats(t: &Theme, total: &Totals) -> Div {
    let tokens = &total.tokens;
    let hit = tokens
        .hit_rate()
        .filter(|_| tokens.cache_read > 0)
        .map(|rate| t!("usage.hit_rate", rate = percent(rate)));
    let written = (tokens.cache_write > 0)
        .then(|| t!("usage.cache_written", count = count(tokens.cache_write)));
    let calls_label = if total.requests == 1 {
        t!("usage.call")
    } else {
        t!("usage.calls")
    };
    stats_card(t)
        .child(tile(
            t,
            0,
            count(tokens.in_out()),
            t!("usage.tokens"),
            Some(t!(
                "usage.in_out",
                input = count(tokens.input),
                output = count(tokens.output)
            )),
            None,
        ))
        .child(tile(
            t,
            1,
            count(tokens.cache_read),
            t!("usage.cache_read"),
            hit,
            written,
        ))
        .child(tile(
            t,
            2,
            count(tokens.reasoning),
            t!("usage.reasoning"),
            Some(t!("usage.inside_output")),
            None,
        ))
        .child(tile(t, 3, grouped(total.requests), calls_label, None, None))
}

fn tile(
    t: &Theme,
    index: usize,
    value: String,
    label: impl Into<SharedString>,
    sub: Option<Cow<'static, str>>,
    tip: Option<Cow<'static, str>>,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    div()
        .id(ElementId::named_usize("tile", index))
        .flex_1()
        .min_w_0()
        .v_flex()
        .bg(t.card)
        .when(index == 0, |d| d.rounded_l(px(11.)))
        .when(index == 3, |d| d.rounded_r(px(11.)))
        .pt(px(12.))
        .pb(px(11.))
        .px(px(14.))
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(23.))
                .child(value),
        )
        .child(
            div()
                .mt(px(3.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.muted)
                .child(label.to_uppercase()),
        )
        .when_some(sub, |d, sub| {
            d.child(
                div()
                    .mt(px(3.))
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .truncate()
                    .child(SharedString::from(sub)),
            )
        })
        .when_some(tip, |d, tip| {
            d.tooltip(ui::tooltip(SharedString::from(tip)))
        })
}

/// 还没扫完：统计块和图表的位置先画灰块。
fn skeleton(t: &Theme) -> Div {
    let block = |w: f32, h: f32| div().w(px(w)).h(px(h)).rounded(px(4.)).bg(t.pill);
    let mut card = stats_card(t);
    for i in 0..4 {
        card = card.child(
            div()
                .flex_1()
                .v_flex()
                .gap(px(8.))
                .bg(t.card)
                .when(i == 0, |d| d.rounded_l(px(11.)))
                .when(i == 3, |d| d.rounded_r(px(11.)))
                .pt(px(14.))
                .pb(px(14.))
                .px(px(14.))
                .child(block(64., 20.))
                .child(block(44., 10.)),
        );
    }
    div().v_flex().child(card).child(
        div()
            .mt(px(8.))
            .h(px(118.))
            .bg(t.card)
            .border_1()
            .border_color(t.line)
            .rounded(px(12.)),
    )
}

/// 柱状图。`switch` 是「全部」时左上角的切换。
fn chart(t: &Theme, summary: &Summary, switch: Option<AnyElement>) -> Div {
    let buckets = &summary.buckets;
    let peak = buckets
        .iter()
        .map(|bucket| bucket.totals.tokens.in_out())
        .max()
        .unwrap_or(0)
        .max(1);
    let n = buckets.len();
    let height = |value: u64| px(CHART_HEIGHT * value as f32 / peak as f32);
    let bars = buckets.iter().enumerate().map(|(i, bucket)| {
        let tokens = &bucket.totals.tokens;
        let (input, output) = (height(tokens.input), height(tokens.output));
        let hover = t.pill;
        div()
            .id(ElementId::named_usize("bar", i))
            .flex_1()
            .min_w(px(2.))
            .h_full()
            .v_flex()
            .justify_end()
            .rounded_t(px(3.))
            .hover(move |s| s.bg(hover))
            .tooltip(ui::tooltip(bar_tip(
                summary.step,
                bucket.start,
                &bucket.totals,
            )))
            .when(output > px(0.), |d| {
                d.child(div().h(output).flex_none().rounded_t(px(3.)).bg(t.accent))
            })
            .when(input > px(0.), |d| {
                d.child(
                    div()
                        .h(input)
                        .flex_none()
                        .when(output <= px(0.), |d| d.rounded_t(px(3.)))
                        .bg(t.accent.opacity(0.38)),
                )
            })
    });
    // 标签：不超过 8 根全标，一个月以内大约 6 个，再多大约 5 个；最后一根离上一个标签够远也标。
    let every = if n <= 8 {
        1
    } else if n <= 31 {
        n.div_ceil(6)
    } else {
        n.div_ceil(5)
    };
    let labels = buckets.iter().enumerate().filter_map(|(i, bucket)| {
        let last = i + 1 == n && (n - 1) % every >= every / 2 && i % every != 0;
        if i % every != 0 && !last {
            return None;
        }
        let label = div()
            .absolute()
            .top_0()
            .whitespace_nowrap()
            .child(when_label(summary.step, bucket.start, false));
        Some(if last {
            label.right_0()
        } else {
            label.left(relative(i as f32 / n as f32))
        })
    });
    chart_card(t)
        .child(
            div()
                .h_flex()
                .items_end()
                .mb(px(5.))
                .text_size(px(10.5))
                .text_color(t.faint)
                .children(switch)
                .child(div().flex_1())
                .child(count(peak)),
        )
        .child(
            div()
                .h_flex()
                .items_end()
                .gap(px(3.))
                .h(px(CHART_HEIGHT))
                .border_b_1()
                .border_color(t.line_2)
                .children(bars),
        )
        .child(
            div()
                .relative()
                .h(px(14.))
                .mt(px(5.))
                .text_size(px(10.5))
                .text_color(t.muted)
                .children(labels),
        )
}

fn chart_card(t: &Theme) -> Div {
    div()
        .mt(px(8.))
        .pt(px(12.))
        .px(px(14.))
        .pb(px(8.))
        .bg(t.card)
        .border_1()
        .border_color(t.line)
        .rounded(px(12.))
}

/// 热力图左边星期那一列的宽度。
const HEAT_LABEL: f32 = 28.;
const HEAT_GAP: f32 = 3.;
/// 格子最大多宽（窗口很宽时不再变大）。
const HEAT_CELL_MAX: f32 = 18.;

/// 格子高度：53 列铺满卡片时大约多宽（页面左右各 16，卡片边框 1、左右留白 14）。宽度由 flex 分，和这个差不到 1。
fn heat_cell(window: &Window) -> f32 {
    let inner = f32::from(window.viewport_size().width) - 32. - 2. - 28.;
    let weeks = HEATMAP_WEEKS as f32;
    ((inner - HEAT_LABEL - weeks * HEAT_GAP) / weeks).clamp(6., HEAT_CELL_MAX)
}

/// 热力图：一列一周，周一在上；每天的深浅按 `metric` 在有用量的那些天里排第几个四分位（magpie 的 `sessLevels`）。
fn heat_grid(t: &Theme, map: &Heatmap, metric: Metric, cell: f32) -> Div {
    let value = |day: &Totals| match metric {
        Metric::Tokens => day.tokens.in_out() as f64,
        Metric::Requests => day.requests as f64,
        Metric::Cost => day.cost,
    };
    let level = levels(&map.days.iter().map(value).collect::<Vec<_>>());
    let colors = [
        t.pill,
        t.pill.blend(t.accent.opacity(0.22)),
        t.pill.blend(t.accent.opacity(0.45)),
        t.pill.blend(t.accent.opacity(0.7)),
        t.accent,
    ];
    let weeks = map.days.len().div_ceil(7);
    let zh = rust_i18n::locale().starts_with("zh");
    // 月份标在这个月第一个周一那一列；离上一个不到 3 周、第一列里这个月已过一半、最后一列，都不标。
    let mut months: Vec<Option<String>> = vec![None; weeks];
    let (mut month, mut last) = (0, None);
    for (i, day) in map.first.iter_days().take(map.days.len()).enumerate() {
        let week = i / 7;
        if i % 7 != 0 || day.month() == month {
            continue;
        }
        month = day.month();
        let room = last.is_none_or(|last| week - last >= 3);
        if room && !(week == 0 && day.day() > 14) && week + 1 < weeks {
            months[week] = Some(if zh {
                format!("{month}月")
            } else {
                day.format("%b").to_string()
            });
            last = Some(week);
        }
    }
    let month_row = div()
        .h_flex()
        .gap(px(HEAT_GAP))
        .h(px(13.))
        .child(div().w(px(HEAT_LABEL)).flex_none())
        .children(months.into_iter().map(|month| {
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .h_full()
                .children(month.map(|month| {
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .whitespace_nowrap()
                        .child(month)
                }))
        }));
    let rows = (0..7).map(|weekday| {
        div()
            .h_flex()
            .gap(px(HEAT_GAP))
            .child(
                div()
                    .w(px(HEAT_LABEL))
                    .flex_none()
                    .when(weekday % 2 == 0, |d| d.child(weekday_name(weekday, zh))),
            )
            .children((0..weeks).map(|week| {
                let i = week * 7 + weekday;
                let cell = div().flex_1().min_w_0().h(px(cell));
                let Some(totals) = map.days.get(i) else {
                    return cell.into_any_element();
                };
                let day = map.first + Duration::days(i as i64);
                cell.id(ElementId::named_usize("heat", i))
                    .rounded(px(3.))
                    .bg(colors[level(value(totals))])
                    .tooltip(ui::tooltip(totals_tip(day_label(day, zh), totals)))
                    .into_any_element()
            }))
    });
    let legend = div()
        .h_flex()
        .justify_end()
        .gap(px(3.))
        .mt(px(5.))
        .child(div().mr(px(3.)).child(t!("usage.less")))
        .children(colors.map(|color| div().size(px(10.)).rounded(px(2.)).bg(color)))
        .child(div().ml(px(3.)).child(t!("usage.more")));
    div()
        .v_flex()
        .gap(px(HEAT_GAP))
        .w_full()
        .max_w(px(HEAT_LABEL + weeks as f32 * (HEAT_CELL_MAX + HEAT_GAP)))
        .text_size(px(10.))
        .text_color(t.muted)
        .child(month_row)
        .children(rows)
        .child(legend)
}

/// 0 是没有用量；1–3 是在有用量的天里排在前 1/4、1/2、3/4 以内，4 是更多的和最多的那天。
fn levels(values: &[f64]) -> impl Fn(f64) -> usize + use<> {
    let mut used: Vec<f64> = values.iter().copied().filter(|v| *v > 0.).collect();
    used.sort_by(f64::total_cmp);
    let at = |p: f64| {
        let i = ((p * used.len() as f64) as usize).min(used.len().saturating_sub(1));
        used.get(i).copied().unwrap_or(0.)
    };
    let (a, b, c) = (at(0.25), at(0.5), at(0.75));
    let top = used.last().copied().unwrap_or(0.);
    move |v| {
        if v <= 0. {
            0
        } else if v >= top {
            4
        } else if v <= a {
            1
        } else if v <= b {
            2
        } else if v <= c {
            3
        } else {
            4
        }
    }
}

fn weekday_name(weekday: usize, zh: bool) -> &'static str {
    const ZH: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    const EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    if zh { ZH[weekday] } else { EN[weekday] }
}

/// 热力图悬停提示里的日期：带年份和星期。
fn day_label(day: NaiveDate, zh: bool) -> String {
    let weekday = weekday_name(day.weekday().num_days_from_monday() as usize, zh);
    if zh {
        format!("{}年{}月{}日 {weekday}", day.year(), day.month(), day.day())
    } else {
        day.format("%a, %b %-d, %Y").to_string()
    }
}

/// 柱子下面的标签（`long` 是悬停提示里的：按周的写「某天那周」）。
fn when_label(step: Step, start: i64, long: bool) -> String {
    let Some(time) = Local.timestamp_millis_opt(start).earliest() else {
        return String::new();
    };
    let day = if rust_i18n::locale().starts_with("zh") {
        time.format("%-m月%-d日").to_string()
    } else {
        time.format("%b %-d").to_string()
    };
    match step {
        Step::Hour => time.format("%H:00").to_string(),
        Step::Day => day,
        Step::Week if long => t!("usage.week_of", date = day).to_string(),
        Step::Week => day,
    }
}

fn bar_tip(step: Step, start: i64, totals: &Totals) -> String {
    totals_tip(when_label(step, start, true), totals)
}

/// 「某时 · 多少 token · 几次调用 · ≈花费」。
fn totals_tip(when: String, totals: &Totals) -> String {
    if totals.requests == 0 {
        return t!("usage.bar_empty", when = when).to_string();
    }
    let mut text = t!(
        "usage.bar",
        when = when,
        tokens = count(totals.tokens.in_out()),
        calls = calls_text(totals.requests)
    )
    .to_string();
    if let Some(cost) = cost_text(totals) {
        text.push_str(&format!(" · ≈{cost}"));
    }
    text
}

/// 列表里的一行要画的东西。
struct Line {
    logo: Logo,
    name: SharedString,
    /// 名字是「未知」这类说明，画淡一点。
    faint: bool,
    /// 名字下面、调用次数前面的一句。
    note: Option<SharedString>,
}

fn sources_list(
    t: &Theme,
    summary: &Summary,
    loaded: &Loaded,
    store: &Store,
    window: &Window,
    cx: &App,
) -> Div {
    let lines = summary.sources.iter().map(|(source, totals)| {
        let line = match source {
            Source::Provider(id) => {
                let provider = store.providers.iter().find(|p| &p.id == id);
                let name = provider
                    .map(|p| p.name.clone())
                    .or_else(|| loaded.names.get(id).cloned())
                    .unwrap_or_else(|| id.clone());
                Line {
                    logo: provider.map_or_else(|| Logo::letter(&name), data::logo),
                    name: name.into(),
                    faint: false,
                    note: None,
                }
            }
            Source::Official(agent) => {
                let vendor = usage::vendor(*agent).and_then(preset::get);
                Line {
                    logo: vendor
                        .and_then(|p| data::preset_logo(&p.id))
                        .unwrap_or(Logo::letter("?")),
                    name: vendor
                        .map_or_else(|| agent.id().to_string(), |p| p.name.clone())
                        .into(),
                    faint: false,
                    note: Some(t!("usage.official").into()),
                }
            }
            // agent 自己配的（Pi 的 `/login`、自己写的 `models.json`）：名字是预设 ID 的用预设的名字；
            // 图标按 ID 的第一段认（`openai-codex` 是 OpenAI）。名字下面写是哪个 agent 的。
            Source::Other(agent, name) => {
                let display = preset::get(name).map_or_else(|| name.clone(), |p| p.name.clone());
                Line {
                    logo: data::preset_logo(name).unwrap_or_else(|| Logo::letter(name)),
                    name: display.into(),
                    faint: false,
                    note: store
                        .agents
                        .iter()
                        .find(|a| a.kind == *agent)
                        .map(|a| a.name.into()),
                }
            }
            Source::Unknown => Line {
                logo: Logo::Letter('?'),
                name: t!("usage.unknown").into(),
                faint: true,
                note: None,
            },
        };
        (line, totals)
    });
    stat_list(t, "source", &summary.total, lines, window, cx)
}

fn agents_list(t: &Theme, summary: &Summary, store: &Store, window: &Window, cx: &App) -> Div {
    let lines = summary.agents.iter().map(|(kind, totals)| {
        let agent = store.agents.iter().find(|a| a.kind == *kind);
        let line = Line {
            logo: agent.map_or(Logo::letter(kind.id()), |a| a.logo),
            name: agent.map_or(kind.id(), |a| a.name).into(),
            faint: false,
            note: None,
        };
        (line, totals)
    });
    stat_list(t, "agent", &summary.total, lines, window, cx)
}

fn models_list(t: &Theme, summary: &Summary, window: &Window, cx: &App) -> Div {
    let lines = summary.models.iter().map(|(model, totals)| {
        let line = if model.is_empty() {
            Line {
                logo: Logo::Letter('?'),
                name: t!("usage.unknown").into(),
                faint: true,
                note: None,
            }
        } else {
            Line {
                logo: model_logo(model),
                name: model.clone().into(),
                faint: false,
                note: None,
            }
        };
        (line, totals)
    });
    stat_list(t, "model", &summary.total, lines, window, cx)
}

/// 模型名看得出是哪家的，用那家的图标。
fn model_logo(model: &str) -> Logo {
    let lower = model.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));
    let vendor = if starts(&["claude"]) {
        "anthropic"
    } else if starts(&["gpt", "codex", "chatgpt"])
        || (name.starts_with('o') && name[1..].starts_with(|c: char| c.is_ascii_digit()))
    {
        "openai"
    } else if starts(&["grok"]) {
        "xai"
    } else if starts(&["deepseek"]) {
        "deepseek"
    } else if starts(&["glm", "chatglm"]) {
        "zhipu"
    } else if starts(&["kimi", "moonshot", "k2", "k3"]) {
        "kimi"
    } else if starts(&["qwen", "qwq"]) {
        "qianwen"
    } else if starts(&["minimax", "abab"]) {
        "minimax"
    } else if starts(&["gemini", "gemma"]) {
        "google"
    } else if starts(&["doubao", "seed"]) {
        "volcengine"
    } else if starts(&["ling-", "ring-"]) {
        "ant"
    } else if starts(&["mistral", "codestral", "devstral", "magistral"]) {
        "mistral"
    } else if starts(&["llama"]) {
        "meta"
    } else {
        ""
    };
    data::preset_logo(vendor).unwrap_or_else(|| Logo::letter(model))
}

fn stat_list<'a>(
    t: &Theme,
    id: &'static str,
    total: &Totals,
    lines: impl Iterator<Item = (Line, &'a Totals)>,
    window: &Window,
    cx: &App,
) -> Div {
    let whole = total.tokens.in_out().max(1) as f32;
    let mut list = ui::list(t, false);
    for (i, (line, totals)) in lines.enumerate() {
        let tokens = &totals.tokens;
        let share = tokens.in_out() as f32 / whole;
        let mut sub: Vec<String> = line.note.iter().map(ToString::to_string).collect();
        sub.push(calls_text(totals.requests));
        list = list.child(
            ui::row(t, false, i == 0)
                .child(ui::logo(line.logo, 18., t, window, cx))
                .child(
                    div()
                        .v_flex()
                        .flex_none()
                        .w(px(180.))
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .when(line.faint, |d| d.text_color(t.faint))
                                .when(!line.faint, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(line.name),
                        )
                        .child(
                            div()
                                .mt(px(1.))
                                .text_size(px(11.5))
                                .text_color(t.muted)
                                .truncate()
                                .child(sub.join(" · ")),
                        ),
                )
                .child(
                    div()
                        .id(ElementId::Name(format!("{id}-share-{i}").into()))
                        .flex_1()
                        .h(px(4.))
                        .mx(px(6.))
                        .rounded(px(2.))
                        .bg(t.pill)
                        .tooltip(ui::tooltip(t!(
                            "usage.share",
                            percent = (share * 100.).round() as u64
                        )))
                        .child(
                            div()
                                .h_full()
                                .w(relative(share.max(0.015)))
                                .rounded(px(2.))
                                .bg(t.accent.opacity(0.7)),
                        ),
                )
                // 定宽：各行的占比条右端对齐。
                .child(
                    div()
                        .v_flex()
                        .items_end()
                        .flex_none()
                        .w(px(NUM_WIDTH))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(count(tokens.in_out())),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(t.muted)
                                .whitespace_nowrap()
                                .child(detail_text(tokens)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .flex_none()
                        .min_w(px(58.))
                        .text_size(px(12.))
                        .text_color(t.green)
                        .children(cost_text(totals).map(|cost| format!("≈{cost}"))),
                ),
        );
    }
    list
}

/// 「输入 X · 输出 Y · 缓存 Z · 命中率 N%」
fn detail_text(tokens: &Tokens) -> String {
    let mut text = t!(
        "usage.in_out",
        input = count(tokens.input),
        output = count(tokens.output)
    )
    .to_string();
    if tokens.cache_read > 0 {
        text.push_str(" · ");
        text.push_str(&t!("usage.cached", count = count(tokens.cache_read)));
        if let Some(rate) = tokens.hit_rate() {
            text.push_str(" · ");
            text.push_str(&t!("usage.hit_rate", rate = percent(rate)));
        }
    }
    text
}

fn calls_text(calls: u64) -> String {
    if calls == 1 {
        t!("usage.calls_one").to_string()
    } else {
        t!("usage.calls_other", count = grouped(calls)).to_string()
    }
}

fn unpriced_text(calls: u64) -> String {
    if calls == 1 {
        t!("usage.unpriced_one").to_string()
    } else {
        t!("usage.unpriced_other", count = grouped(calls)).to_string()
    }
}

/// 花费：上百不要小数，上一块两位，一块以下三位；有没价格的请求加个「+」。都没价格是 `None`。
fn cost_text(totals: &Totals) -> Option<String> {
    if totals.cost == 0. && totals.unpriced > 0 {
        return None;
    }
    let cost = totals.cost;
    let digits = if cost >= 100. {
        0
    } else if cost >= 1. {
        2
    } else {
        3
    };
    let plus = if totals.unpriced > 0 { "+" } else { "" };
    Some(format!("${cost:.digits$}{plus}"))
}

/// token 数：1.2K、23M、1.05B（magpie 的 `fmtN`）。
fn count(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if n >= 1e7 {
        format!("{:.0}M", n / 1e6)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e5 {
        format!("{:.0}K", n / 1e3)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{n}")
    }
}

/// 调用次数：千位加逗号。
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn percent(rate: f64) -> String {
    format!("{}%", (rate * 100.).round() as u64)
}

#[cfg(test)]
mod tests {
    use confdeck_core::usage::summary::Totals;

    use super::{cost_text, count, grouped};

    #[test]
    fn numbers_read_like_magpie() {
        assert_eq!(count(999), "999");
        assert_eq!(count(2_200), "2.2K");
        assert_eq!(count(158_000), "158K");
        assert_eq!(count(2_200_000), "2.2M");
        assert_eq!(count(23_400_000), "23M");
        assert_eq!(count(1_050_000_000), "1.05B");
        assert_eq!(grouped(705), "705");
        assert_eq!(grouped(172_887), "172,887");
        assert_eq!(grouped(1_000_000), "1,000,000");
    }

    #[test]
    fn costs_round_by_size() {
        let totals = |cost, unpriced| Totals {
            cost,
            unpriced,
            ..Totals::default()
        };
        assert_eq!(cost_text(&totals(76.594, 0)).as_deref(), Some("$76.59"));
        assert_eq!(cost_text(&totals(1363.96, 0)).as_deref(), Some("$1364"));
        assert_eq!(cost_text(&totals(0.0312, 2)).as_deref(), Some("$0.031+"));
        assert_eq!(cost_text(&totals(0., 3)), None);
        assert_eq!(cost_text(&totals(0., 0)).as_deref(), Some("$0.000"));
    }
}
