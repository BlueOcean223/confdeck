//! 选供应商的浮层，照 magpie 的 model picker：顶部是过滤框，下面是选项列表。
//! 打开就能直接打字过滤；上下键移动高亮、回车选定、Esc 关闭；鼠标悬停也会移动高亮。

use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::data::Store;
use crate::theme::Theme;
use crate::ui;

pub enum PickerEvent {
    /// 选了一项：`None` 是「默认」，交还给 agent 自带的登录。
    Picked(Option<usize>),
    Dismissed,
}

pub struct Picker {
    store: Entity<Store>,
    agent: usize,
    panel: bool,
    input: Entity<InputState>,
    highlighted: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for Picker {}

/// 没有匹配项时那行提示的高度。
const EMPTY_HEIGHT: f32 = 56.;

/// 过滤后的选项。「默认」总在第一个（过滤词能匹配到它时）。
fn options(store: &Store, query: &str) -> Vec<Option<usize>> {
    let query = query.trim().to_lowercase();
    let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
    let mut options = Vec::new();
    if matches("默认 default 官方") {
        options.push(None);
    }
    options.extend(
        store
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| matches(p.name) || matches(p.host))
            .map(|(i, _)| Some(i)),
    );
    options
}

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
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("筛选供应商…"));
        input.update(cx, |input, cx| input.focus(window, cx));

        let current = store.read(cx).agents[agent].provider;
        let highlighted = options(store.read(cx), "")
            .iter()
            .position(|o| *o == current)
            .unwrap_or(0);

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
                    this.highlighted = 0;
                    cx.notify();
                }
            }),
            intercept,
        ];

        Self {
            store,
            agent,
            panel,
            input,
            highlighted,
            scroll: ScrollHandle::new(),
            _subscriptions,
        }
    }

    fn options(&self, cx: &App) -> Vec<Option<usize>> {
        options(self.store.read(cx), &self.input.read(cx).value())
    }

    /// 整个选择器有多高（点），面板按它调整窗口高度。数字和 `render` 里的尺寸一一对应：
    /// 过滤框 39，列表上下各 5 的内边距、分组标题 24、每行 34、最高 300，外圈描边 2。
    pub fn height(&self, cx: &App) -> f32 {
        let options = self.options(cx);
        let list = if options.is_empty() {
            EMPTY_HEIGHT
        } else {
            let label = if options.iter().any(Option::is_some) { 24. } else { 0. };
            label + options.len() as f32 * 34.
        };
        39. + (list + 10.).min(300.) + 2.
    }

    fn handle_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let options = self.options(cx);
        let count = options.len();
        match key {
            "up" if count > 0 => self.highlighted = (self.highlighted + count - 1) % count,
            "down" if count > 0 => self.highlighted = (self.highlighted + 1) % count,
            "enter" => {
                if let Some(option) = options.get(self.highlighted) {
                    cx.emit(PickerEvent::Picked(*option));
                }
                return true;
            }
            "escape" => {
                cx.emit(PickerEvent::Dismissed);
                return true;
            }
            _ => return false,
        }
        self.scroll_to_highlighted(&options);
        cx.notify();
        true
    }

    /// 列表的子元素里夹着分组标题，所以要换算成子元素的下标。
    fn scroll_to_highlighted(&self, options: &[Option<usize>]) {
        let has_default = options.first() == Some(&None);
        let header = usize::from(options.iter().any(Option::is_some));
        let child = if has_default && self.highlighted == 0 {
            0
        } else {
            self.highlighted + header
        };
        self.scroll.scroll_to_item(child);
    }

    fn option_row(
        &self,
        n: usize,
        option: Option<usize>,
        store: &Store,
        t: &Theme,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let agent = &store.agents[self.agent];
        let (logo, name, sub): (_, &str, String) = match option {
            Some(p) => {
                let provider = &store.providers[p];
                (provider.logo, provider.name, provider.host.to_string())
            }
            None => (agent.logo, "默认", format!("使用 {} 自带的登录", agent.name)),
        };
        let current = agent.provider == option;

        div()
            .id(ElementId::named_usize("option", n))
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(n == self.highlighted, |row| row.bg(t.sel))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.highlighted != n {
                    this.highlighted = n;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PickerEvent::Picked(option))))
            .child(ui::logo(logo, 16., t))
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .min_w_0()
                    .flex_1()
                    .child(div().flex_none().text_size(px(12.5)).child(name.to_string()))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(sub),
                    ),
            )
            .when(current, |row| row.child(ui::icon("check", 13., t.accent)))
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = if self.panel {
            Theme::panel(window.appearance())
        } else {
            Theme::window(window.appearance())
        };
        let options = self.options(cx);
        let store = self.store.read(cx);

        let mut list = div()
            .id("picker-list")
            .v_flex()
            .p(px(5.))
            .max_h(px(300.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        let mut header_shown = false;
        for (n, option) in options.iter().enumerate() {
            if option.is_some() && !header_shown {
                header_shown = true;
                list = list.child(ui::group_label(&t, "供应商"));
            }
            list = list.child(self.option_row(n, *option, store, &t, cx));
        }
        if options.is_empty() {
            list = list.child(
                div()
                    .h(px(EMPTY_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .child("没有匹配的供应商"),
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
                            .child(
                                Input::new(&self.input)
                                    .appearance(false)
                                    .text_size(px(13.)),
                            ),
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
