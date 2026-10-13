//! 主窗口，布局照 magpie：左上是标志，顶栏中间是分段标签，下面是圆角卡片列表。

use std::time::Duration;

use confdeck_core::provider::Provider;
use confdeck_core::quota::CheckKey;
use confdeck_core::readings;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::add_provider::{AddEvent, AddProvider};
use crate::check_sheet::{CheckEvent, CheckSheet};
use crate::config_page::{self, ConfigEvent, ConfigPage};
use crate::data::{self, Notice, NoticeKind, Store};
use crate::editor::{Editor, EditorEvent};
use crate::net;
use crate::official::{self, Officials, OfficialsEvent};
use crate::picker::{Picker, PickerEvent};
use crate::quota_view;
use crate::sessions_page::SessionsPage;
use crate::settings_page::SettingsPage;
use crate::theme::Theme;
use crate::ui;
use crate::usage_page::UsagePage;

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
    TABS.iter()
        .map(|(tab, key)| (*tab, t!(*key).into()))
        .collect()
}

/// 主窗口的句柄和里面的视图（窗口的根视图是 gpui-kit 的 `Root`，从它拿不到 `MainWindow`）。
/// 窗口关掉后句柄失效，再打开时会新建。
#[derive(Default)]
struct MainWindowHandle {
    window: Option<AnyWindowHandle>,
    view: Option<WeakEntity<MainWindow>>,
}

impl Global for MainWindowHandle {}

/// 主窗口开着时返回它。
#[cfg(debug_assertions)]
pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<MainWindowHandle>().and_then(|h| h.window)
}

/// 打开面板前调：应用在后台时，打开面板要激活应用，激活会把开着的主窗口一起带到前面，这里让它留在原位。
pub fn stay_behind(cx: &mut App) {
    #[cfg(target_os = "macos")]
    if !crate::quiet()
        && let Some(handle) = cx.try_global::<MainWindowHandle>().and_then(|h| h.window)
    {
        handle
            .update(cx, |_, window, cx| {
                crate::mac::stay_behind_on_activation(window, cx)
            })
            .ok();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// 有主窗口就提到前台，没有就新开一个。
pub fn show(store: Entity<Store>, cx: &mut App) {
    crate::activate(cx);
    let existing = cx.default_global::<MainWindowHandle>().window;
    if let Some(handle) = existing
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }

    let (handle, view) = gpui_kit::open_window(MainWindow::options(cx), cx, |window, cx| {
        // 点关闭按钮只收起窗口、不销毁，下次 `show` 原样拿出来：销毁时 GPUI 释放窗口的 content view，
        // AppKit 的 TouchBar 观察者还挂在上面，下一次刷新就崩（notes/panel-focus-crash.md「第四次」）。
        #[cfg(target_os = "macos")]
        window.on_window_should_close(cx, |window, cx| {
            crate::mac::order_out(window, cx);
            false
        });
        cx.new(|cx| MainWindow::new(store, window, cx))
    })
    .expect("failed to open main window");
    *cx.global_mut::<MainWindowHandle>() = MainWindowHandle {
        window: Some(handle),
        view: Some(view.downgrade()),
    };
}

/// 面板里点了「把当前配置存成供应商」：打开主窗口，盖上预先填好的新建表单；存好后把这个 agent 切过去。
pub fn show_draft(store: Entity<Store>, agent: usize, draft: Provider, cx: &mut App) {
    show(store, cx);
    let global = cx.default_global::<MainWindowHandle>();
    let (Some(handle), Some(main)) = (
        global.window,
        global.view.as_ref().and_then(WeakEntity::upgrade),
    ) else {
        return;
    };
    handle
        .update(cx, |_, window, cx| {
            main.update(cx, |main, cx| main.open_draft(agent, draft, window, cx));
        })
        .ok();
}

struct OpenPicker {
    agent: usize,
    picker: Entity<Picker>,
    _subscription: Subscription,
}

/// Agent 页上点了「配置 ›」：这一页换成那个 agent 的配置页，面包屑回来。
struct OpenConfig {
    page: Entity<ConfigPage>,
    _subscription: Subscription,
}

/// 盖在页面上的弹层：添加供应商的卡片表、一家供应商的编辑器，或「额度查询」。
enum Overlay {
    Check {
        sheet: Entity<CheckSheet>,
        _subscription: Subscription,
    },
    Add {
        sheet: Entity<AddProvider>,
        _subscription: Subscription,
    },
    Edit {
        editor: Entity<Editor>,
        _subscription: Subscription,
    },
}

struct Toast {
    id: u64,
    kind: NoticeKind,
    text: SharedString,
}

/// 成功提示多久后消失。
const TOAST_LIFETIME: Duration = Duration::from_secs(5);

pub struct MainWindow {
    store: Entity<Store>,
    tab: Tab,
    picker: Option<OpenPicker>,
    config: Option<OpenConfig>,
    /// 顶栏齿轮打开的设置页，盖在当前标签的页面上；再点齿轮或点标签就关。
    settings: Option<Entity<SettingsPage>>,
    /// 用量页：第一次点开时建，之后留着（选的时间段不丢）。
    usage: Option<Entity<UsagePage>>,
    sessions: Option<Entity<SessionsPage>>,
    overlay: Option<Overlay>,
    /// 供应商页最上面官方那一组（账号摊在里面）。
    officials: Entity<Officials>,
    /// 供应商行上打开了「⋯」的那家（ID）；刚被「点外面」关掉的那家（点的正好是它的「⋯」时不要立刻再打开）。
    provider_menu: Option<String>,
    provider_menu_closed: Option<String>,
    toasts: Vec<Toast>,
    toast_seq: u64,
    /// 刚被「点外面」关掉的选择器。点的正好是它的下拉框时，不要立刻再打开。
    just_closed: Option<usize>,
    /// 上一个标签和这次切换的编号，给分段标签的滑块放动画。
    tab_anim: Option<(usize, u64)>,
    /// 页面的滚动区（各标签共用一个）。
    page_scroll: ScrollHandle,
    clicks: u64,
    should_move: bool,
    _subscriptions: Vec<Subscription>,
}

impl MainWindow {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        crate::hush(window);
        let readings = store.read(cx).readings.clone();
        let _subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            // 额度、余额查回来只通知画读数的地方（`readings.rs`）。
            cx.observe(&readings, |_, _, cx| cx.notify()),
            // 写了文件（这个窗口、面板或启动时的后台任务）都从这里收到一句话，做成提示条。
            cx.subscribe(&store, |this, _, notice: &Notice, cx| {
                this.toast(notice.kind, notice.text.clone(), cx)
            }),
            // 系统切了深浅色：gpui-component 的主题（代码编辑框的底色、行号栏）也跟着切，不然要等下次打开配置页。
            cx.observe_window_appearance(window, |_, window, cx| {
                crate::theme::sync_component_theme(&Theme::window(window.appearance()), cx);
                cx.notify();
            }),
            // 切回这个窗口时在后台重读一次：别处改了配置（编辑器里手改、agent 自己写），胶囊和配置页跟上。
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.store.update(cx, |store, cx| store.reload(cx));
                }
            }),
        ];
        let officials = cx.new(|cx| Officials::new(store.clone(), window, cx));
        let mut _subscriptions = _subscriptions;
        _subscriptions.push(cx.subscribe_in(
            &officials,
            window,
            |this, _, event: &OfficialsEvent, window, cx| match event {
                OfficialsEvent::Check { target, name, logo } => {
                    this.open_check(target.clone(), name.clone(), *logo, window, cx)
                }
            },
        ));
        Self {
            store,
            tab: Tab::Agents,
            picker: None,
            config: None,
            settings: None,
            usage: None,
            sessions: None,
            overlay: None,
            officials,
            provider_menu: None,
            provider_menu_closed: None,
            toasts: Vec::new(),
            toast_seq: 0,
            just_closed: None,
            tab_anim: None,
            page_scroll: ScrollHandle::new(),
            clicks: 0,
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
                        cx.listener(|this, tab: &Tab, window, cx| {
                            if this.tab == *tab {
                                return;
                            }
                            if this.config_dirty(cx) {
                                return;
                            }
                            let from = TABS.iter().position(|(t, _)| *t == this.tab).unwrap_or(0);
                            this.clicks += 1;
                            this.tab_anim = Some((from, this.clicks));
                            this.tab = *tab;
                            this.picker = None;
                            this.config = None;
                            this.settings = None;
                            match *tab {
                                Tab::Providers => {
                                    let readings = this.store.read(cx).readings.clone();
                                    readings.update(cx, |readings, cx| readings.check_due(cx))
                                }
                                Tab::Usage => this.open_usage(cx),
                                Tab::Sessions => this.open_sessions(window, cx),
                                _ => {}
                            }
                            cx.notify();
                        }),
                    )),
            )
            .child(div().flex_1())
            .child(
                ui::icon_button_with(
                    "refresh",
                    t,
                    ui::spinning(
                        ui::icon("rotate-cw", 16., t.ctl_fg),
                        "refresh-spin",
                        self.tab == Tab::Providers
                            && self.store.read(cx).readings.read(cx).checking_any(),
                    ),
                )
                .on_click(cx.listener(|this, _, _, cx| match this.tab {
                    Tab::Providers => {
                        let readings = this.store.read(cx).readings.clone();
                        readings.update(cx, |readings, cx| readings.check_all(cx))
                    }
                    Tab::Usage => this.store.update(cx, |store, cx| store.refresh_usage(cx)),
                    Tab::Sessions => this.store.update(cx, |store, cx| store.refresh_usage(cx)),
                    _ => {}
                })),
            )
            .child(
                ui::icon_button("settings", t, "settings")
                    .when(self.settings.is_some(), |b| b.bg(t.pill_hover))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx))),
            )
    }

    /// 切到用量页：第一次建页面（建的时候扫一遍），之后每次切过来扫一遍新加的。
    fn open_usage(&mut self, cx: &mut Context<Self>) {
        match &self.usage {
            Some(_) => self.store.update(cx, |store, cx| store.refresh_usage(cx)),
            None => {
                let store = self.store.clone();
                self.usage = Some(cx.new(|cx| UsagePage::new(store, cx)));
            }
        }
    }

    /// 切到会话页：第一次建页面，之后每次切过来扫一遍新加的（点开的会话还开着）。
    fn open_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.sessions {
            Some(_) => self.store.update(cx, |store, cx| store.refresh_usage(cx)),
            None => {
                let store = self.store.clone();
                self.sessions = Some(cx.new(|cx| SessionsPage::new(store, window, cx)));
            }
        }
    }

    /// 齿轮：开着就关，关着就开。
    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.settings = None;
        } else {
            let store = self.store.clone();
            self.settings = Some(cx.new(|cx| SettingsPage::new(store, window, cx)));
            self.picker = None;
        }
        cx.notify();
    }

    /// 设置页「显示哪些 agent」里没关掉的都列出来；配置文件不在的一样列，只是没有胶囊。
    fn agents_page(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let shown: Vec<usize> = (0..store.agents.len())
            .filter(|&i| !store.agents[i].hidden_on_page)
            .collect();

        let mut list = ui::list(t, false);
        for (n, &i) in shown.iter().enumerate() {
            list = list.child(self.agent_row(i, n == 0, store, t, window, cx));
        }
        // 全关掉了就什么都不画（空卡片只剩一圈边）。
        div()
            .v_flex()
            .gap(px(10.))
            .when(!shown.is_empty(), |page| page.child(list))
    }

    fn open_picker(&mut self, agent: usize, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let picker = cx.new(|cx| Picker::new(store, agent, false, window, cx));
        // 选了就写是选择器自己的事（结果从 Store 的 Notice 事件来），这里只管「存成供应商」和关掉。
        let _subscription = cx.subscribe_in(
            &picker,
            window,
            move |this, _, event: &PickerEvent, window, cx| {
                if let PickerEvent::SaveAs(draft) = event {
                    this.open_draft(agent, draft.clone(), window, cx);
                }
                this.picker = None;
                cx.notify();
            },
        );
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
        let status: SharedString = if agent.installed() {
            agent.config_path().into()
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
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(agent.name),
                    )
                    .child(
                        div()
                            .mt(px(1.))
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .truncate()
                            .child(status),
                    ),
            )
            .when(agent.selectable(), |row| {
                row.child(
                    ui::text_button(ElementId::named_usize("config", i), t)
                        .mr(px(14.))
                        .child(t!("agents.configure"))
                        .child(ui::icon("chevron-right", 11., t.faint))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.open_config(i, window, cx)),
                        ),
                )
                .child(
                    // 要放得下「DeepSeek · Key 不一致」（预览 09 的胶囊是 228）。
                    div()
                        .relative()
                        .w(px(224.))
                        .child(
                            ui::select_pill(
                                ElementId::named_usize("pick", i),
                                t,
                                false,
                                &logos,
                                label,
                                open.is_some(),
                                store.pill_dashed(i),
                                window,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.just_closed == Some(i) {
                                        return;
                                    }
                                    if this.picker.as_ref().is_some_and(|p| p.agent == i) {
                                        this.picker = None;
                                        cx.notify();
                                    } else {
                                        this.open_picker(i, window, cx);
                                    }
                                },
                            )),
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

    /// 「配置 ›」：Agent 页换成这个 agent 的配置页。面包屑回来；选择器里的「存成供应商…」和 Agent 页一样处理。
    fn open_config(&mut self, agent: usize, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let page = cx.new(|cx| ConfigPage::new(store, agent, window, cx));
        let _subscription = cx.subscribe_in(
            &page,
            window,
            move |this, _, event: &ConfigEvent, window, cx| match event {
                ConfigEvent::Back => {
                    if this.config_dirty(cx) {
                        return;
                    }
                    this.config = None;
                    cx.notify();
                }
                ConfigEvent::SaveAs(draft) => this.open_draft(agent, draft.clone(), window, cx),
                ConfigEvent::Reveal(top, bottom) => this.reveal(*top, *bottom, cx),
            },
        );
        self.picker = None;
        self.config = Some(OpenConfig {
            page,
            _subscription,
        });
        cx.notify();
    }

    /// 配置页有没保存的改动：不让离开，让它的底栏晃一下。设置页盖在上面时不算离开。
    fn config_dirty(&self, cx: &mut Context<Self>) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        if self.settings.is_some() || !config.page.read(cx).dirty(cx) {
            return false;
        }
        config.page.update(cx, |page, cx| page.nudge(cx));
        true
    }

    /// 页面滚到能看见窗口里 `top..bottom` 这一段（配置页编辑框里的光标）；底下浮着保存栏时让开它。
    /// 滚过头 GPUI 布局时会收回来。
    fn reveal(&mut self, top: Pixels, bottom: Pixels, cx: &mut Context<Self>) {
        let view = self.page_scroll.bounds();
        let dirty = self
            .config
            .as_ref()
            .is_some_and(|config| config.page.read(cx).dirty(cx));
        let below = px(if dirty {
            config_page::SAVE_BAR_SPACE
        } else {
            16.
        });
        let above = px(16.);
        let mut offset = self.page_scroll.offset();
        if bottom > view.bottom() - below {
            offset.y -= bottom - (view.bottom() - below);
        } else if top < view.top() + above {
            offset.y = (offset.y + view.top() + above - top).min(px(0.));
        } else {
            return;
        }
        self.page_scroll.set_offset(offset);
        cx.notify();
    }

    /// 配置页的「保存 / 取消」底栏（设置页盖在上面时不画）。
    fn save_bar(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.settings.is_some() || self.tab != Tab::Agents {
            return None;
        }
        let page = &self.config.as_ref()?.page;
        page.update(cx, |page, cx| page.save_bar(t, cx))
    }

    fn providers_page(
        &self,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let count = store.providers.len();
        let mut page = div().v_flex().gap(px(10.));

        if let Some(error) = &store.load_error {
            page = page.child(
                div()
                    .h_flex()
                    .items_start()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(t.danger.opacity(0.1))
                    .text_size(px(12.))
                    .text_color(t.danger)
                    .child(ui::icon("circle-alert", 14., t.danger).mt(px(1.)))
                    .child(t!("providers.load_error", error = error.clone())),
            );
        }

        page = page.child(
            div()
                .h_flex()
                .items_center()
                .px(px(4.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.muted)
                        .child(if count == 1 {
                            t!("providers.count_one")
                        } else {
                            t!("providers.count_other", count = count)
                        }),
                )
                .child(div().flex_1())
                .when(store.load_error.is_none() && count > 0, |row| {
                    row.child(if store.checking() {
                        div()
                            .mr(px(16.))
                            .text_size(px(12.5))
                            .text_color(t.muted)
                            .child(t!("providers.pinging"))
                            .into_any_element()
                    } else {
                        ui::text_button("ping-all", t)
                            .mr(px(16.))
                            .child(ui::icon("activity", 13., t.ctl_fg))
                            .child(t!("providers.ping"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.check_reach(store.providers.clone(), cx)
                                })
                            }))
                            .into_any_element()
                    })
                })
                .when(store.load_error.is_none(), |row| {
                    row.child(
                        ui::text_button("add-provider", t)
                            .child(ui::icon("plus", 13., t.ctl_fg))
                            .child(t!("providers.add"))
                            .on_click(cx.listener(|this, _, window, cx| this.open_add(window, cx))),
                    )
                }),
        );

        // 有额度、余额可写时，名字一列定宽、额度一列紧跟在后面，各行（官方的和各家的）的额度格子上下对齐。
        let officials = store.officials();
        let readings = store.readings.read(cx);
        let reading = |provider: &Provider| readings.of(&CheckKey::Provider(provider.id.clone()));
        let meters = !officials.is_empty() || store.providers.iter().any(|p| reading(p).is_some());

        if !officials.is_empty() {
            page = page.child(self.officials.clone());
        }

        if store.providers.is_empty() {
            return page.child(
                ui::list(t, false).child(
                    div()
                        .v_flex()
                        .items_center()
                        .gap(px(4.))
                        .py(px(32.))
                        .child(div().text_color(t.muted).child(t!("providers.empty"))),
                ),
            );
        }

        let mut list = ui::list(t, false);
        for (i, provider) in store.providers.iter().enumerate() {
            let last = i + 1 == count;
            let host = data::host(provider).unwrap_or_default().to_string();
            let models = provider.models.len();
            let sub: SharedString = if models == 1 {
                t!("providers.summary_one", host = host).into()
            } else {
                t!("providers.summary_other", host = host, count = models).into()
            };
            let users = store.users_of(i);
            // 测过速才有：最慢的往返时间或不通的原因；几个地址不一样时悬停列出各自的结果。
            let reach = store.provider_reach(provider).map(|(all, each)| {
                let text = net::reach_text(t, ElementId::named_usize("reach", i), &all).mr(px(6.));
                if each.len() > 1 {
                    let lines: Vec<String> = each
                        .iter()
                        .map(|(label, reach)| format!("{label}  {}", net::reach_line(reach)))
                        .collect();
                    text.tooltip(ui::tooltip(lines.join("\n")))
                } else {
                    text
                }
            });
            let hover_bg = t.card_2;
            let provider_clone = provider.clone();

            let key_pill = match data::key_tail(provider) {
                Some(tail) => div()
                    .font_family("Menlo")
                    .text_size(px(11.5))
                    .text_color(t.fg_2)
                    .bg(t.pill)
                    .child(tail),
                None => div()
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .border_1()
                    .border_color(t.line)
                    .child(t!("providers.no_key")),
            }
            .flex_none()
            .h(px(22.))
            .px(px(8.))
            .rounded(px(6.))
            .flex()
            .items_center();

            let reading = reading(provider);
            list = list.child(
                ui::row(t, false, i == 0)
                    .id(ElementId::named_usize("provider", i))
                    .when(i == 0, |row| row.rounded_t(px(11.)))
                    .when(last, |row| row.rounded_b(px(11.)))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(provider_clone.clone(), window, cx)
                    }))
                    .child(ui::logo(data::logo(provider), 22., t, window, cx))
                    .child(
                        official::name_column(meters)
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .child(provider.name.clone()),
                            )
                            .child(
                                div()
                                    .mt(px(1.))
                                    .text_size(px(11.5))
                                    .text_color(t.muted)
                                    .truncate()
                                    .child(sub),
                            ),
                    )
                    .when(meters, |row| {
                        row.child(match &reading {
                            Some(reading) => quota_view::cells(
                                t,
                                &format!("provider-{i}"),
                                reading,
                                &store.readings,
                            ),
                            None => quota_view::blank(),
                        })
                        .child(div().flex_1())
                    })
                    // 在用的 agent 只画小图标，悬停显示「Claude Code · 模型名」，不响应点击（提案 10）。
                    .when(!users.is_empty(), |row| {
                        row.child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap(px(6.))
                                .mr(px(6.))
                                .children(users.iter().map(|&a| {
                                    let agent = &store.agents[a];
                                    let tip = match store.model_for(a, i) {
                                        Some(model) => format!("{} · {model}", agent.name),
                                        None => agent.name.to_string(),
                                    };
                                    official::user_chip(
                                        t,
                                        format!("user-{i}-{a}"),
                                        agent.logo,
                                        tip,
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .children(reach)
                    .child(key_pill)
                    .child(self.provider_more(i, provider, t, cx)),
            );
        }

        page.child(list)
    }

    fn open_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let sheet = cx.new(|cx| AddProvider::new(store, window, cx));
        let _subscription = cx.subscribe_in(
            &sheet,
            window,
            |this, _, event: &AddEvent, window, cx| match event {
                AddEvent::Chosen(preset) => {
                    let store = this.store.clone();
                    let preset = *preset;
                    let editor = cx.new(|cx| Editor::create(store, preset, window, cx));
                    this.show_editor(editor, None, cx);
                }
                // 账号登录：关掉卡片表，回到页面最上面官方那一组。不起登录，账号跟着 agent 自己的登录走。
                AddEvent::Official => {
                    this.overlay = None;
                    this.page_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
                AddEvent::Closed => {
                    this.overlay = None;
                    cx.notify();
                }
            },
        );
        self.overlay = Some(Overlay::Add {
            sheet,
            _subscription,
        });
        cx.notify();
    }

    fn open_editor(
        &mut self,
        provider: confdeck_core::provider::Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let editor = cx.new(|cx| Editor::edit(store, provider, window, cx));
        self.show_editor(editor, None, cx);
    }

    /// 选择器里的「把当前配置存成供应商…」：按 agent 的配置文件预先填好的新建表单。
    /// 存好后把这个 agent 切到新建的那家：文件里写了的不变，没写的槽位按目录补（core.md「存成供应商」）。
    fn open_draft(
        &mut self,
        agent: usize,
        draft: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let editor = cx.new(|cx| Editor::from_draft(store, draft, window, cx));
        self.show_editor(editor, Some(agent), cx);
    }

    fn show_editor(
        &mut self,
        editor: Entity<Editor>,
        then_switch: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        // 存好、删掉的提示从 Store 的 Notice 事件来。
        let _subscription = cx.subscribe(&editor, move |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::Saved { id } = event {
                let saved = this
                    .store
                    .read(cx)
                    .providers
                    .iter()
                    .position(|p| &p.id == id);
                if let (Some(agent), Some(provider)) = (then_switch, saved) {
                    this.store
                        .update(cx, |s, cx| s.switch(agent, Some(provider), cx));
                }
            }
            this.overlay = None;
            cx.notify();
        });
        self.overlay = Some(Overlay::Edit {
            editor,
            _subscription,
        });
        cx.notify();
    }

    fn toast(&mut self, kind: NoticeKind, text: SharedString, cx: &mut Context<Self>) {
        self.toast_seq += 1;
        let id = self.toast_seq;
        self.toasts.push(Toast { id, kind, text });
        if kind == NoticeKind::Success {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TOAST_LIFETIME).await;
                this.update(cx, |this, cx| this.dismiss_toast(id, cx)).ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        self.toasts.retain(|toast| toast.id != id);
        cx.notify();
    }

    /// 供应商行尾的「⋯」：编辑、额度查询（查得了的）、删除（打开编辑器问一句）。
    fn provider_more(
        &self,
        i: usize,
        provider: &confdeck_core::provider::Provider,
        t: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let open = self.provider_menu.as_deref() == Some(provider.id.as_str());
        let id = provider.id.clone();
        let more = ui::more_button(ElementId::named_usize("provider-more", i), t, open)
            .ml(px(2.))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if this.provider_menu_closed.take().as_deref() == Some(id.as_str()) {
                    return;
                }
                this.provider_menu = if this.provider_menu.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id.clone())
                };
                cx.notify();
            }));
        if !open {
            return more.into_any_element();
        }
        let target = CheckKey::Provider(provider.id.clone());
        let checkable = readings::checkable(&target);
        let (edit, remove) = (provider.clone(), provider.clone());
        let name: SharedString = provider.name.clone().into();
        let logo = data::logo(provider);
        let closed = provider.id.clone();
        let menu = ui::menu(t)
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                this.provider_menu = None;
                this.provider_menu_closed = Some(closed.clone());
                cx.notify();
            }))
            .child(
                ui::menu_item("provider-edit", t, t!("providers.edit"), false).on_click(
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.provider_menu = None;
                        this.open_editor(edit.clone(), window, cx);
                    }),
                ),
            )
            .when(checkable, |menu| {
                menu.child(
                    ui::menu_item("provider-check", t, t!("check.title"), false).on_click(
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.provider_menu = None;
                            this.open_check(target.clone(), name.clone(), logo, window, cx);
                        }),
                    ),
                )
            })
            .child(ui::menu_divider(t))
            .child(
                ui::menu_item("provider-remove", t, t!("accounts.remove"), true).on_click(
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.provider_menu = None;
                        this.open_editor(remove.clone(), window, cx);
                        if let Some(Overlay::Edit { editor, .. }) = &this.overlay {
                            editor.update(cx, |editor, cx| editor.ask_delete(cx));
                        }
                    }),
                ),
            );
        div()
            .relative()
            .child(more)
            .child(ui::dropdown(menu))
            .into_any_element()
    }

    /// 「额度查询」：一家供应商或一个官方账号的查询设置，盖在页面上。
    fn open_check(
        &mut self,
        target: CheckKey,
        name: SharedString,
        logo: data::Logo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let sheet = cx.new(|cx| CheckSheet::new(store, target, name, logo, window, cx));
        let _subscription = cx.subscribe(&sheet, |this, _, event: &CheckEvent, cx| match event {
            CheckEvent::Closed => {
                this.overlay = None;
                cx.notify();
            }
        });
        self.overlay = Some(Overlay::Check {
            sheet,
            _subscription,
        });
        cx.notify();
    }

    /// 点弹层外面：编辑器有改动会先问，卡片表、额度查询直接关。
    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::Edit { editor, .. }) => {
                editor.update(cx, |editor, cx| editor.request_close(cx));
            }
            Some(Overlay::Add { .. }) | Some(Overlay::Check { .. }) => {
                self.overlay = None;
                cx.notify();
            }
            None => {}
        }
    }

    fn overlay_layer(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let content: AnyElement = match self.overlay.as_ref()? {
            Overlay::Add { sheet, .. } => sheet.clone().into_any_element(),
            Overlay::Edit { editor, .. } => editor.clone().into_any_element(),
            Overlay::Check { sheet, .. } => sheet.clone().into_any_element(),
        };
        Some(
            deferred(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_start()
                    .justify_center()
                    .pt(px(36.))
                    .bg(t.backdrop)
                    .occlude()
                    .child(
                        div()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_overlay(cx)))
                            .child(content)
                            // 出现时淡入、从上方落下（和选择器一样）。
                            .with_animation(
                                "overlay-card",
                                Animation::new(Duration::from_millis(260))
                                    .with_easing(ui::ease_out_quart),
                                |el, d| el.opacity((d * 1.6).min(1.)).mt(px(-10. * (1. - d))),
                            ),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// `above_bar`：配置页的底栏浮着，提示条往上让开。
    fn toast_layer(
        &self,
        t: &Theme,
        above_bar: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let bottom = if above_bar {
            config_page::SAVE_BAR_SPACE
        } else {
            14.
        };
        let toasts = self.toasts.iter().map(|toast| {
            let id = toast.id;
            let (icon, color) = match toast.kind {
                NoticeKind::Success => ("circle-check", t.success),
                NoticeKind::Error => ("circle-alert", t.danger),
            };
            div()
                .id(ElementId::NamedInteger("toast".into(), id))
                .h_flex()
                .items_center()
                .gap(px(8.))
                .max_w(px(620.))
                .pl(px(12.))
                .pr(px(if toast.kind == NoticeKind::Error {
                    8.
                } else {
                    14.
                }))
                .py(px(9.))
                .rounded(px(10.))
                .bg(Hsla { a: 1., ..t.pop_bg })
                .border_1()
                .border_color(t.line)
                .shadow(t.pop_shadow())
                .text_size(px(12.5))
                .text_color(t.fg)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.dismiss_toast(id, cx)))
                .child(ui::icon(icon, 15., color))
                .child(div().min_w_0().child(toast.text.clone()))
                .when(toast.kind == NoticeKind::Error, |d| {
                    d.child(ui::icon("x", 13., t.muted).ml(px(4.)))
                })
                .with_animation(
                    ElementId::NamedInteger("toast-in".into(), id),
                    Animation::new(Duration::from_millis(240)).with_easing(ui::ease_out_quart),
                    |el, d| el.opacity(d).mt(px(8. * (1. - d))),
                )
        });
        Some(
            deferred(
                div()
                    .absolute()
                    .bottom(px(bottom))
                    .left_0()
                    .right_0()
                    .v_flex()
                    .items_center()
                    .gap(px(8.))
                    .children(toasts),
            )
            .with_priority(3)
            .into_any_element(),
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
        let save_bar = self.save_bar(&t, cx);
        let toasts = self.toast_layer(&t, save_bar.is_some(), cx);
        let page = match (&self.settings, self.tab, &self.config) {
            (Some(settings), _, _) => settings.clone().into_any_element(),
            (None, Tab::Agents, Some(config)) => config.page.clone().into_any_element(),
            (None, Tab::Agents, None) => self.agents_page(&t, window, cx).into_any_element(),
            (None, Tab::Providers, _) => self.providers_page(&t, window, cx).into_any_element(),
            (None, Tab::Usage, _) if self.usage.is_some() => self
                .usage
                .clone()
                .map(|usage| usage.into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            (None, Tab::Sessions, _) if self.sessions.is_some() => self
                .sessions
                .clone()
                .map(|sessions| sessions.into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            _ => self.placeholder(&t).into_any_element(),
        };

        div()
            .relative()
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
                    .track_scroll(&self.page_scroll)
                    .px(px(16.))
                    .pb(px(16.))
                    .child(page),
            )
            .children(save_bar)
            .children(self.overlay_layer(&t, cx))
            .children(toasts)
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
                assert!(
                    backend.translate(locale, key).is_some(),
                    "{locale} is missing {key}"
                );
            }
        }
    }
}
