//! 选供应商的浮层，照 magpie 的 model picker：顶部是过滤框，下面是选项列表。
//!
//! - 单选 agent：「默认」在最前，选一项就关。
//! - 多选 agent：每行前面有勾选框，勾一下写一次、浮层不关，可以连着勾（提案 01）。
//!   打开时勾着的排在「写进 X 的配置」一组，其余在「可添加」；之后勾、取消只改勾选框，不重新排，行不会在鼠标底下跳。
//! - 接口对不上的供应商排在最后、置灰、写明缺哪种接口，选不了（提案 01）。
//!
//! 打开就能直接打字过滤；上下键移动高亮、回车选定（多选时是勾 / 取消）、Esc 关闭；鼠标悬停也会移动高亮。
//! 多选时过滤框是空的，空格也能勾 / 取消；有过滤词时空格照常输入（供应商名里有空格）。

use std::time::Duration;

use confdeck_core::agent::Mode;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Selection, Store, Unusable};
use crate::theme::Theme;
use crate::ui;

pub enum PickerEvent {
    /// 单选 agent 选了一项：`None` 是「默认」，交还给 agent 自带的登录。
    Picked(Option<usize>),
    /// 多选 agent 勾上或取消了一家，浮层不关。
    Toggled(usize),
    Dismissed,
}

/// 列表里的一行。
enum Row {
    Header(SharedString),
    Option {
        /// `None` 是「默认」。
        option: Option<usize>,
        /// 接口对不上的原因；有原因的置灰、选不了。
        reason: Option<SharedString>,
    },
}

impl Row {
    fn selectable(&self) -> Option<Option<usize>> {
        match self {
            Row::Option {
                option,
                reason: None,
            } => Some(*option),
            _ => None,
        }
    }

    fn height(&self) -> f32 {
        match self {
            Row::Header(_) => 24.,
            Row::Option { .. } => 34.,
        }
    }
}

pub struct Picker {
    store: Entity<Store>,
    agent: usize,
    panel: bool,
    multi: bool,
    /// 多选 agent 打开时勾着的几家，决定分组。
    pinned: Vec<usize>,
    input: Entity<InputState>,
    /// 高亮的是 `rows()` 里第几行，只会落在选得了的行上。
    highlighted: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for Picker {}

/// 没有匹配项时那行提示的高度。
const EMPTY_HEIGHT: f32 = 56.;

impl Picker {
    pub fn new(
        store: Entity<Store>,
        agent: usize,
        panel: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = if panel {
            Theme::panel(window.appearance())
        } else {
            Theme::window(window.appearance())
        };
        crate::theme::sync_component_theme(&t, cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("picker.filter")));
        input.update(cx, |input, cx| input.focus(window, cx));

        let (multi, pinned) = match &store.read(cx).agents[agent].selection {
            Selection::Single(_) => (false, Vec::new()),
            Selection::Multi(enabled) => (true, enabled.clone()),
        };
        debug_assert_eq!(multi, store.read(cx).agents[agent].mode == Mode::Coexist);

        // 过滤框本身绑定了上下键、回车和 Esc，会先把这几个键吃掉，
        // 所以在按键绑定生效之前就拦下来。
        let picker = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
            {
                return;
            }
            let Some(picker) = picker.upgrade() else {
                return;
            };
            let key = event.keystroke.key.clone();
            if picker.update(cx, |picker, cx| picker.handle_key(&key, cx)) {
                cx.stop_propagation();
            }
        });

        let _subscriptions = vec![
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.highlight_first(cx);
                    cx.notify();
                }
            }),
            // 多选时勾了一家，勾选框要跟着变。
            cx.observe(&store, |_, _, cx| cx.notify()),
            intercept,
        ];

        let mut picker = Self {
            store,
            agent,
            panel,
            multi,
            pinned,
            input,
            highlighted: 0,
            scroll: ScrollHandle::new(),
            _subscriptions,
        };
        // 单选时高亮现在用的那家，多选时高亮第一行。
        let rows = picker.rows(cx);
        let current = match &picker.store.read(cx).agents[agent].selection {
            Selection::Single(current) => Some(*current),
            Selection::Multi(_) => None,
        };
        picker.highlighted = rows
            .iter()
            .position(|row| current.is_some() && row.selectable() == current)
            .or_else(|| rows.iter().position(|row| row.selectable().is_some()))
            .unwrap_or(0);
        picker
    }

    /// 过滤后的行。单选：「默认」、能用的供应商、接口对不上的；多选：勾着的、可添加的、接口对不上的。
    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let agent = &store.agents[self.agent];
        let query = self.input.read(cx).value().trim().to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);

        let mut rows = Vec::new();
        if !self.multi && matches(&t!("picker.default_keywords")) {
            rows.push(Row::Option {
                option: None,
                reason: None,
            });
        }

        let shown: Vec<usize> = (0..store.providers.len())
            .filter(|&p| {
                let provider = &store.providers[p];
                matches(&provider.name) || data::host(provider).is_some_and(matches)
            })
            .collect();
        let (usable, unusable): (Vec<usize>, Vec<usize>) = shown
            .iter()
            .partition(|&&p| store.unusable(self.agent, p).is_none());

        let group = |rows: &mut Vec<Row>, label: SharedString, providers: &[usize]| {
            if providers.is_empty() {
                return;
            }
            rows.push(Row::Header(label));
            rows.extend(providers.iter().map(|&p| Row::Option {
                option: Some(p),
                reason: None,
            }));
        };
        if self.multi {
            let (pinned, rest): (Vec<usize>, Vec<usize>) =
                usable.iter().partition(|p| self.pinned.contains(p));
            group(
                &mut rows,
                t!("picker.in_config", agent = agent.name).into(),
                &pinned,
            );
            group(&mut rows, t!("picker.available").into(), &rest);
        } else {
            group(&mut rows, t!("picker.group").into(), &usable);
        }
        if !unusable.is_empty() {
            let protocols = agent
                .protocols
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(&t!("common.list_separator"));
            let incompatible: SharedString =
                t!("picker.incompatible", protocols = protocols).into();
            let no_key: SharedString = t!("picker.no_key").into();
            if rows.is_empty() || self.multi {
                rows.push(Row::Header(t!("picker.incompatible_group").into()));
            }
            rows.extend(unusable.into_iter().map(|p| Row::Option {
                option: Some(p),
                reason: Some(match store.unusable(self.agent, p) {
                    Some(Unusable::NoKey) => no_key.clone(),
                    _ => incompatible.clone(),
                }),
            }));
        }
        rows
    }

    /// 整个选择器有多高（点），面板按它调整窗口高度。数字和 `render` 里的尺寸一一对应：
    /// 过滤框 39，列表上下各 5 的内边距、分组标题 24、每行 34、最高 300，外圈描边 2。
    pub fn height(&self, cx: &App) -> f32 {
        let rows = self.rows(cx);
        let list = if rows.is_empty() {
            EMPTY_HEIGHT
        } else {
            rows.iter().map(Row::height).sum()
        };
        39. + (list + 10.).min(300.) + 2.
    }

    fn highlight_first(&mut self, cx: &App) {
        self.highlighted = self
            .rows(cx)
            .iter()
            .position(|row| row.selectable().is_some())
            .unwrap_or(0);
    }

    /// 从 `from` 往 `step` 方向找下一个选得了的行，到头绕回去。
    fn step(rows: &[Row], from: usize, step: isize) -> usize {
        let count = rows.len() as isize;
        let mut at = from as isize;
        for _ in 0..count {
            at = (at + step).rem_euclid(count);
            if rows[at as usize].selectable().is_some() {
                return at as usize;
            }
        }
        from
    }

    fn activate(&mut self, option: Option<usize>, cx: &mut Context<Self>) {
        match (self.multi, option) {
            (true, Some(p)) => cx.emit(PickerEvent::Toggled(p)),
            (true, None) => {}
            (false, option) => cx.emit(PickerEvent::Picked(option)),
        }
    }

    fn handle_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let rows = self.rows(cx);
        match key {
            "up" if !rows.is_empty() => self.highlighted = Self::step(&rows, self.highlighted, -1),
            "down" if !rows.is_empty() => self.highlighted = Self::step(&rows, self.highlighted, 1),
            "enter" => {
                if let Some(option) = rows.get(self.highlighted).and_then(Row::selectable) {
                    self.activate(option, cx);
                }
                return true;
            }
            "space" if self.multi && self.input.read(cx).value().is_empty() => {
                if let Some(option) = rows.get(self.highlighted).and_then(Row::selectable) {
                    self.activate(option, cx);
                }
                return true;
            }
            "escape" => {
                cx.emit(PickerEvent::Dismissed);
                return true;
            }
            _ => return false,
        }
        self.scroll.scroll_to_item(self.highlighted);
        cx.notify();
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn option_row(
        &self,
        n: usize,
        option: Option<usize>,
        reason: Option<SharedString>,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let agent = &store.agents[self.agent];
        let (logo, name, sub): (Logo, SharedString, SharedString) = match option {
            Some(p) => {
                let provider = &store.providers[p];
                let host = data::host(provider).unwrap_or_default();
                let sub = match (&reason, store.protocol_for(self.agent, p)) {
                    (Some(reason), _) => reason.clone(),
                    // 多协议的 agent 才需要说明走哪种协议（decisions「供应商和 agent 的关系」）。
                    (None, Some(protocol)) if agent.protocols.len() > 1 => {
                        format!("{host} · {protocol}").into()
                    }
                    (None, _) => host.to_string().into(),
                };
                (data::logo(provider), provider.name.clone().into(), sub)
            }
            None => (
                agent.logo,
                t!("common.default").into(),
                t!("picker.default_hint", agent = agent.name).into(),
            ),
        };
        let disabled = reason.is_some();
        let checked = option.is_some_and(|p| store.is_enabled(self.agent, p));

        div()
            .id(ElementId::named_usize("option", n))
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .when(disabled, |row| row.opacity(0.5))
            .when(!disabled, |row| {
                row.cursor_pointer()
                    .when(n == self.highlighted, |row| row.bg(t.sel))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.highlighted != n {
                            this.highlighted = n;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.activate(option, cx)))
            })
            .when(self.multi, |row| row.child(ui::checkbox(t, checked)))
            .child(ui::logo(logo, 16., t, window, cx))
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .min_w_0()
                    .flex_1()
                    .child(div().flex_none().text_size(px(12.5)).child(name))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(sub),
                    ),
            )
            .when(!self.multi && checked, |row| {
                row.child(ui::icon("check", 13., t.accent))
            })
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = if self.panel {
            Theme::panel(window.appearance())
        } else {
            Theme::window(window.appearance())
        };
        let rows = self.rows(cx);
        let store = self.store.read(cx);

        let mut list = div()
            .id("picker-list")
            .v_flex()
            .p(px(5.))
            .max_h(px(300.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for (n, row) in rows.iter().enumerate() {
            list = match row {
                Row::Header(label) => list.child(ui::group_label(&t, label.clone())),
                Row::Option { option, reason } => {
                    list.child(self.option_row(n, *option, reason.clone(), store, &t, window, cx))
                }
            };
        }
        if rows.is_empty() {
            list = list.child(
                div()
                    .h(px(EMPTY_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .child(t!("picker.empty")),
            );
        }

        // magpie 的浮层是半透明加背景模糊；GPUI 的元素做不了背景模糊，半透明会让底下的行
        // 透出来，所以用不透明的底色。
        div()
            .v_flex()
            .w_full()
            .bg(Hsla { a: 1., ..t.pop_bg })
            .rounded(px(11.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .overflow_hidden()
            .occlude()
            .text_color(t.fg)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(39.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(t.line_2)
                    .child(ui::icon("search", 14., t.faint))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .child(Input::new(&self.input).appearance(false).text_size(px(13.))),
                    ),
            )
            .child(list)
            // 出现时淡入并从上方落下来（magpie 的 pop-fade + pop-in）。
            .with_animation(
                "picker-in",
                Animation::new(Duration::from_millis(320)).with_easing(ui::ease_out_quart),
                |el, delta| el.opacity((delta * 2.2).min(1.)).mt(px(-6. * (1. - delta))),
            )
    }
}
