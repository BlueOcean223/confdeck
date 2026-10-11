//! 主窗口，布局照 magpie：左上是标志，顶栏中间是分段标签，下面是圆角卡片列表。

use std::borrow::Cow;

use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::Store;
use crate::picker::{Picker, PickerEvent};
use crate::theme::Theme;
use crate::ui;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Agents,
    Providers,
    Mcp,
    Prompts,
    Usage,
    Sessions,
}

/// 每个标签和它文案的键。
const TABS: &[(Tab, &str)] = &[
    (Tab::Agents, "tabs.agents"),
    (Tab::Providers, "tabs.providers"),
    (Tab::Mcp, "tabs.mcp"),
    (Tab::Prompts, "tabs.prompts"),
    (Tab::Usage, "tabs.usage"),
    (Tab::Sessions, "tabs.sessions"),
];

/// 标签和当前语言的文案。
fn tabs() -> Vec<(Tab, SharedString)> {
    TABS.iter().map(|(tab, key)| (*tab, t!(*key).into())).collect()
}

/// 主窗口的句柄。窗口关掉后句柄失效，再打开时会新建。
#[derive(Default)]
struct MainWindowHandle(Option<AnyWindowHandle>);

impl Global for MainWindowHandle {}

/// 主窗口开着时返回它。
#[cfg(debug_assertions)]
pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<MainWindowHandle>().and_then(|h| h.0)
}

/// 有主窗口就提到前台，没有就新开一个。
pub fn show(store: Entity<Store>, cx: &mut App) {
    cx.activate(true);
    let existing = cx.default_global::<MainWindowHandle>().0;
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }

    let (handle, _) =
        gpui_kit::open_window(MainWindow::options(cx), cx, |window, cx| {
            cx.new(|cx| MainWindow::new(store, window, cx))
        })
        .expect("failed to open main window");
    cx.global_mut::<MainWindowHandle>().0 = Some(handle);
}

struct OpenPicker {
    agent: usize,
    picker: Entity<Picker>,
    _subscription: Subscription,
}

pub struct MainWindow {
    store: Entity<Store>,
    tab: Tab,
    picker: Option<OpenPicker>,
    /// 刚被「点外面」关掉的选择器。点的正好是它的下拉框时，不要立刻再打开。
    just_closed: Option<usize>,
    /// 上一个标签和这次切换的编号，给分段标签的滑块放动画。
    tab_anim: Option<(usize, u64)>,
    clicks: u64,
    show_more: bool,
    should_move: bool,
    _subscriptions: Vec<Subscription>,
}

impl MainWindow {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            tab: Tab::Agents,
            picker: None,
            just_closed: None,
            tab_anim: None,
            clicks: 0,
            show_more: false,
            should_move: false,
            _subscriptions,
        }
    }

    fn options(cx: &App) -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(960.), px(620.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("confdeck".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(17.))),
            }),
            // 顶栏自己处理拖动（见 header），AppKit 不要再把它当成标题栏。
            app_owns_titlebar_drag: true,
            window_min_size: Some(size(px(720.), px(480.))),
            ..Default::default()
        }
    }

    fn header(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .flex_none()
            .h(px(50.))
            .h_flex()
            .items_center()
            .pl(px(84.))
            .pr(px(12.))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = false),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::icon("confdeck", 18., t.fg))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("confdeck"),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::seg(
                        t,
                        &tabs(),
                        self.tab,
                        self.tab_anim,
                        window,
                        cx.listener(|this, tab: &Tab, _, cx| {
                            if this.tab == *tab {
                                return;
                            }
                            let from = TABS.iter().position(|(t, _)| *t == this.tab).unwrap_or(0);
                            this.clicks += 1;
                            this.tab_anim = Some((from, this.clicks));
                            this.tab = *tab;
                            this.picker = None;
                            cx.notify();
                        }),
                    )),
            )
            .child(div().flex_1())
            .child(ui::icon_button("refresh", t, "rotate-cw"))
            .child(ui::icon_button("settings", t, "settings"))
    }

    fn agents_page(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let (installed, others): (Vec<usize>, Vec<usize>) =
            (0..store.agents.len()).partition(|&i| store.agents[i].installed);
        let hidden = others.len();

        let mut list = ui::list(t, false);
        for (n, &i) in installed.iter().enumerate() {
            list = list.child(self.agent_row(i, n == 0, store, t, window, cx));
        }
        if hidden > 0 {
            let hover_bg = t.card_2;
            let hover_fg = t.fg;
            list = list.child(
                div()
                    .id("more")
                    .h_flex()
                    .justify_center()
                    .gap(px(5.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .when(!self.show_more, |row| row.rounded_b(px(11.)))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_more = !this.show_more;
                        cx.notify();
                    }))
                    .child(if self.show_more {
                        t!("agents.show_less")
                    } else {
                        t!("agents.show_more", count = hidden)
                    })
                    .child(ui::icon(
                        if self.show_more { "chevron-up" } else { "chevron-down" },
                        12.,
                        t.muted,
                    )),
            );
            if self.show_more {
                for &i in &others {
                    list = list.child(self.agent_row(i, false, store, t, window, cx));
                }
            }
        }

        div().v_flex().gap(px(10.)).child(list)
    }

    fn open_picker(&mut self, agent: usize, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let picker = cx.new(|cx| Picker::new(store, agent, false, window, cx));
        let _subscription = cx.subscribe(&picker, move |this, _, event: &PickerEvent, cx| {
            match event {
                PickerEvent::Picked(option) => {
                    let option = *option;
                    this.store.update(cx, |s, cx| s.set_provider(agent, option, cx));
                }
                // 勾一下写一次，浮层不关（提案 01）。
                PickerEvent::Toggled(provider) => {
                    let provider = *provider;
                    this.store.update(cx, |s, cx| s.toggle_provider(agent, provider, cx));
                    return;
                }
                PickerEvent::Dismissed => {}
            }
            this.picker = None;
            cx.notify();
        });
        self.picker = Some(OpenPicker {
            agent,
            picker,
            _subscription,
        });
        cx.notify();
    }

    fn agent_row(
        &self,
        i: usize,
        first: bool,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let agent = &store.agents[i];
        let open = self.picker.as_ref().filter(|p| p.agent == i);
        let (logos, label) = store.pill(i);
        // 状态行只写文件在哪。连到哪、是不是 confdeck 写的，都在胶囊里（decisions「没有『接管』」）。
        let status: SharedString = if agent.installed {
            agent.config.into()
        } else {
            t!("agents.no_config").into()
        };

        ui::row(t, false, first)
            .child(ui::logo(agent.logo, 22., t, window, cx))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().font_weight(FontWeight::MEDIUM).truncate().child(agent.name))
                    .child(
                        div()
                            .mt(px(1.))
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .truncate()
                            .child(status),
                    ),
            )
            .when(agent.installed, |row| {
                row.child(
                    ui::text_button(ElementId::named_usize("config", i), t)
                        .mr(px(14.))
                        .child(t!("agents.configure"))
                        .child(ui::icon("chevron-right", 11., t.faint)),
                )
                .child(
                    div()
                        .relative()
                        .w(px(188.))
                        .child(
                            ui::select_pill(
                                ElementId::named_usize("pick", i),
                                t,
                                false,
                                &logos,
                                label,
                                open.is_some(),
                                window,
                                cx,
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.just_closed == Some(i) {
                                    return;
                                }
                                if this.picker.as_ref().is_some_and(|p| p.agent == i) {
                                    this.picker = None;
                                    cx.notify();
                                } else {
                                    this.open_picker(i, window, cx);
                                }
                            })),
                        )
                        .when_some(open, |d, open| {
                            d.child(ui::dropdown(
                                div()
                                    .w(px(320.))
                                    .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                                        this.picker = None;
                                        this.just_closed = Some(i);
                                        cx.notify();
                                    }))
                                    .child(open.picker.clone()),
                            ))
                        }),
                )
            })
    }

    fn providers_page(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let mut list = ui::list(t, false);
        for (i, provider) in store.providers.iter().enumerate() {
            let users = store.users_of(i);
            let sub = if users.is_empty() {
                Cow::Borrowed(provider.host)
            } else {
                let agents = users.join(&t!("common.list_separator"));
                t!("providers.used_by", host = provider.host, agents = agents)
            };
            list = list.child(
                ui::row(t, false, i == 0)
                    .child(ui::logo(provider.logo, 22., t, window, cx))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::MEDIUM).child(provider.name))
                            .child(
                                div()
                                    .mt(px(1.))
                                    .text_size(px(11.5))
                                    .text_color(t.muted)
                                    .truncate()
                                    .child(sub),
                            ),
                    )
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_size(px(12.))
                            .text_color(t.muted)
                            .child(provider.key),
                    )
                    .child(ui::icon("chevron-right", 13., t.faint).ml(px(10.))),
            );
        }

        div().v_flex().gap(px(10.)).child(list).child(
            ui::text_button("add-provider", t)
                .pl(px(12.))
                .child(ui::icon("plus", 13., t.ctl_fg))
                .child(t!("providers.add")),
        )
    }

    fn placeholder(&self, t: &Theme) -> impl IntoElement {
        let name: SharedString = TABS
            .iter()
            .find(|(tab, _)| *tab == self.tab)
            .map(|(_, key)| t!(*key).into())
            .unwrap_or_default();
        ui::list(t, false).child(
            div()
                .v_flex()
                .items_center()
                .py(px(36.))
                .child(div().font_weight(FontWeight::MEDIUM).mb(px(4.)).child(name))
                .child(div().text_color(t.muted).child(t!("placeholder.not_built"))),
        )
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let page = match self.tab {
            Tab::Agents => self.agents_page(&t, window, cx).into_any_element(),
            Tab::Providers => self.providers_page(&t, window, cx).into_any_element(),
            _ => self.placeholder(&t).into_any_element(),
        };

        div()
            .size_full()
            .v_flex()
            .bg(t.bg)
            .text_color(t.fg)
            .text_size(px(13.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = None),
            )
            .child(self.header(&t, window, cx))
            .child(
                div()
                    .id("page")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(16.))
                    .pb(px(16.))
                    .child(page),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::TABS;
    use crate::i18n::LOCALES;

    /// 标签的键是变量传给 `t!` 的，i18n 模块扫源码的测试找不到它们，这里单独查。
    #[test]
    fn tab_labels_are_translated() {
        let backend = crate::_rust_i18n_backend();
        for (_, key) in TABS {
            for locale in LOCALES {
                assert!(backend.translate(locale, key).is_some(), "{locale} is missing {key}");
            }
        }
    }
}
