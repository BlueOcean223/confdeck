//! 菜单栏面板：左键点托盘图标时，在图标正下方弹出（照 magpie 的 quick panel）。
//! 毛玻璃底、圆角；失去焦点或按 Esc 就关掉。

use std::time::{Duration, Instant};

use gpui_kit::component::*;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Notice, NoticeKind, Store};
use crate::main_window;
use crate::picker::{Picker, PickerEvent};
use crate::quota_view::{self, Reading, Shown};
use crate::readings::Readings;
use crate::theme::Theme;
use crate::ui;

actions!(panel, [Dismiss]);

pub const WIDTH: f32 = 440.;

/// 面板离托盘图标的距离（magpie 的 `WindowOffset(6)`）。
const OFFSET: f32 = 6.;
const PADDING: f32 = 12.;
const GAP: f32 = 10.;
const HEADER: f32 = 30.;
const ROW: f32 = 40.;
/// 名字下面多一行额度的行（选的那家能查额度、余额时）。
const ROW_WITH_READING: f32 = 52.;
const LINKS: f32 = 20.;

/// 打开面板之前在前台的应用（macOS），关面板时把前台还给它。
#[cfg(target_os = "macos")]
type ReturnTo = Option<crate::mac::OtherApp>;
#[cfg(not(target_os = "macos"))]
type ReturnTo = ();

/// 调试构建里把面板的开关、焦点变化打到标准错误，查「点了图标面板不出来」这类问题。
macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        eprintln!("panel: {} {}", crate::panel::ms(), format_args!($($arg)*));
    };
}

/// 调试日志的时间戳：毫秒，只留后五位，看两行之间隔了多久。
#[cfg(debug_assertions)]
pub(crate) fn ms() -> u128 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| d.as_millis() % 100_000)
}

/// 面板窗口第一次打开时建好，之后关掉只是藏起来（`mac::hide`），下次打开再显示，不销毁。
#[derive(Default)]
struct PanelState {
    handle: Option<WindowHandle<Panel>>,
    shown: bool,
    /// 从后台打开、在等应用激活完成（`mac::after_activation`）。
    opening: bool,
    closed_at: Option<Instant>,
    /// 从后台打开时原来在前台的应用：关面板时把前台还给它（`close`）。
    return_to: ReturnTo,
}

impl Global for PanelState {}

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some("Panel"))]);
}

/// 面板开着时返回它。
#[cfg(debug_assertions)]
pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<PanelState>()
        .filter(|state| state.shown)
        .and_then(|state| state.handle)
        .map(Into::into)
}

/// 面板开着就关掉，关着就在 `icon`（托盘图标在屏幕上的位置）下方打开。
pub fn toggle(icon: Bounds<Pixels>, store: Entity<Store>, cx: &mut App) {
    let state = cx.default_global::<PanelState>();
    let (handle, shown, opening) = (state.handle, state.shown, state.opening);
    // 面板开着时点托盘图标，面板会先因失焦关掉，紧接着才收到这次点击；这次就不再打开。
    let just_closed = state
        .closed_at
        .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
    trace!("toggle: shown={shown} opening={opening} just_closed={just_closed}");

    if shown {
        if let Some(handle) = handle {
            handle.update(cx, |_, window, cx| close(window, cx)).ok();
        }
        return;
    }
    if just_closed || opening {
        return;
    }

    // 选的那家的额度、余额：到点了的查。
    let readings = store.read(cx).readings.clone();
    readings.update(cx, |readings, cx| readings.check_due(cx));
    let height = base_height(store.read(cx), readings.read(cx));
    let origin = point(icon.center().x - px(WIDTH / 2.), icon.bottom() + px(OFFSET));
    // 和 Wails 一样先激活应用再显示面板。不激活的话，应用在后台、主窗口又开着时，
    // gpui 为绕开 Cocoa 的一个问题会立刻收走面板刚拿到的焦点，接着 AppKit 抛异常崩溃。
    // 激活会把开着的主窗口一起带到前面：先安排好让它留在原位。
    #[cfg(target_os = "macos")]
    let (return_to, background) = if crate::quiet() {
        (None, false)
    } else {
        (crate::mac::frontmost_other(), crate::mac::in_background())
    };
    #[cfg(not(target_os = "macos"))]
    let (return_to, background) = ((), false);
    main_window::stay_behind(cx);
    crate::activate(cx);
    // 激活是异步的：从后台打开时等激活完成再显示。面板先拿到焦点的话，激活时系统把焦点还给主窗口，
    // 面板一失焦就关了，主窗口还会闪一下（10-13 用户发现）。
    if background {
        cx.global_mut::<PanelState>().opening = true;
        #[cfg(target_os = "macos")]
        crate::mac::after_activation(cx, move |cx| show(origin, height, store, return_to, cx));
    } else {
        show(origin, height, store, return_to, cx);
    }
}

/// 在 `origin` 显示面板：第一次建窗口，之后把藏起来的那个挪过来。
fn show(
    origin: Point<Pixels>,
    height: f32,
    store: Entity<Store>,
    return_to: ReturnTo,
    cx: &mut App,
) {
    let state = cx.default_global::<PanelState>();
    state.opening = false;
    let handle = state.handle;
    trace!("show");
    if let Some(handle) = handle
        && handle
            .update(cx, |panel, window, cx| panel.reopen(origin, window, cx))
            .is_ok()
    {
        let state = cx.global_mut::<PanelState>();
        state.shown = true;
        state.return_to = return_to;
        return;
    }

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            origin,
            size(px(WIDTH), px(height)),
        ))),
        titlebar: None,
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| Panel::new(store, window, cx))
    }) {
        Ok(handle) => {
            let state = cx.global_mut::<PanelState>();
            state.handle = Some(handle);
            state.shown = true;
            state.return_to = return_to;
        }
        Err(err) => eprintln!("failed to open panel: {err:#}"),
    }
}

/// 面板列配置文件在、设置页「显示哪些 agent」里没在菜单栏关掉的 agent。
fn shown_agents(store: &Store) -> Vec<usize> {
    (0..store.agents.len())
        .filter(|&i| {
            let agent = &store.agents[i];
            agent.selectable() && !agent.hidden_in_panel
        })
        .collect()
}

/// 这个 agent 现在选的那家的读数，写在名字下面：关掉的、没查过的不写。
fn selection<'a>(store: &Store, readings: &'a Readings, agent: usize) -> Option<Reading<'a>> {
    readings
        .of(&store.selection_key(agent)?)
        .filter(|reading| !matches!(reading.shown, Shown::Off | Shown::Unchecked))
}

/// 一个 agent 那一行的高度：选的那家能查额度、余额时名字下面多一行。
fn row_height(store: &Store, readings: &Readings, agent: usize) -> f32 {
    if selection(store, readings, agent).is_some() {
        ROW_WITH_READING
    } else {
        ROW
    }
}

/// 不开选择器时的面板高度：顶栏、每个 agent 一行、底部一行链接。
fn base_height(store: &Store, readings: &Readings) -> f32 {
    let rows: f32 = shown_agents(store)
        .into_iter()
        .map(|agent| row_height(store, readings, agent))
        .sum();
    PADDING * 2. + HEADER + GAP + rows + GAP + LINKS
}

/// 第 `n` 行底边到窗口顶部的距离。
fn row_bottom(store: &Store, readings: &Readings, n: usize) -> f32 {
    let rows: f32 = shown_agents(store)
        .into_iter()
        .take(n + 1)
        .map(|agent| row_height(store, readings, agent))
        .sum();
    PADDING + HEADER + GAP + rows
}

struct OpenPicker {
    agent: usize,
    row: usize,
    picker: Entity<Picker>,
    /// 窗口长到装得下选择器以后才画它。窗口高度要下一轮主线程任务才改得动（notes/panel-resize.md），
    /// 先画的话第一帧窗口还是矮的，选择器会被挤到上面盖住列表，下一帧才落回原位，看着就是闪一下。
    drawn: bool,
    _subscriptions: Vec<Subscription>,
}

pub struct Panel {
    store: Entity<Store>,
    focus_handle: FocusHandle,
    picker: Option<OpenPicker>,
    just_closed: Option<usize>,
    /// 最近一次写文件的结果，显示在底栏左边，直到下一次写或面板关掉。
    status: Option<Notice>,
    /// 窗口现在的高度，没变就不去改原生窗口。
    height: f32,
    _subscriptions: Vec<Subscription>,
}

impl Panel {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        crate::hush(window);
        #[cfg(target_os = "macos")]
        {
            crate::mac::round_corners(window, 12.);
            // 第一帧画完后形状才确定，再按圆角重算一次阴影。
            window.on_next_frame(|window, _| crate::mac::round_corners(window, 12.));
        }
        let readings = store.read(cx).readings.clone();
        let height = base_height(store.read(cx), readings.read(cx));
        let _subscriptions = vec![
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.fit_height(window, cx);
                cx.notify();
            }),
            // 读数查回来，选的那家名字下面多一行或少一行。
            cx.observe_in(&readings, window, |this, _, window, cx| {
                this.fit_height(window, cx);
                cx.notify();
            }),
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            // 系统什么时候算面板「看得见」：GPUI 的 display link 从这时才跑。
            cx.observe_window_visibility(window, |_, _visibility, _, _| {
                trace!("visibility={_visibility:?}");
            }),
            // 失焦就关。跑 drive 脚本时应用不在前台（`crate::quiet`），焦点怎么变都和面板无关，只按脚本关。
            cx.observe_window_activation(window, |_, window, cx| {
                trace!("active={}", window.is_window_active());
                if !window.is_window_active() && !crate::quiet() {
                    close(window, cx);
                }
            }),
        ];
        Self {
            store,
            focus_handle,
            picker: None,
            just_closed: None,
            status: None,
            height,
            _subscriptions,
        }
    }

    /// 藏起来的面板重新显示：回到刚打开时的样子，挪到托盘图标下面。
    fn reopen(&mut self, origin: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.just_closed = None;
        self.status = None;
        let store = self.store.read(cx);
        self.height = base_height(store, store.readings.read(cx));
        self.focus_handle.focus(window, cx);
        // 跑 drive 脚本时窗口一直透明（`crate::quiet`）。
        #[cfg(target_os = "macos")]
        crate::mac::show_at(window, origin, self.height as f64, !crate::quiet(), cx);
        cx.notify();
    }

    /// 窗口高度跟着内容走：平时刚好装下列表；开着选择器时往下长，刚好装下选择器。
    fn fit_height(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let readings = store.readings.read(cx);
        let base = base_height(store, readings);
        let height = match &self.picker {
            Some(open) => {
                let picker = open.picker.read(cx).height(cx);
                base.max(row_bottom(store, readings, open.row) + 4. + picker + PADDING)
            }
            None => base,
        };
        if height != self.height {
            self.height = height;
            #[cfg(target_os = "macos")]
            crate::mac::set_height_keep_top(window, height as f64, cx);
        }
    }

    fn open_picker(
        &mut self,
        agent: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        trace!("picker: open {agent}");
        let store = self.store.clone();
        // 写完的那句话放到底栏；主窗口开着的话它自己也会收到。
        let panel = cx.entity().downgrade();
        let picker = cx.new(|cx| {
            Picker::new(store, agent, true, window, cx).report_to(move |notice, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.status = notice;
                        cx.notify();
                    })
                    .ok();
            })
        });
        let _subscriptions = vec![
            cx.subscribe_in(
                &picker,
                window,
                move |this, _, event: &PickerEvent, window, cx| match event {
                    // 表单在主窗口里：关掉面板，打开主窗口盖上预先填好的表单。
                    PickerEvent::SaveAs(draft) => {
                        let draft = draft.clone();
                        close_for_main(window, cx);
                        main_window::show_draft(this.store.clone(), agent, draft, cx);
                    }
                    PickerEvent::Closed => this.close_picker(window, cx),
                },
            ),
            // 过滤后选项变少，窗口跟着缩短。
            cx.observe_in(&picker, window, |this, _, window, cx| {
                this.fit_height(window, cx)
            }),
        ];
        self.picker = Some(OpenPicker {
            agent,
            row,
            picker,
            drawn: false,
            _subscriptions,
        });
        self.fit_height(window, cx);
        cx.notify();
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.fit_height(window, cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn header(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        div()
            .h_flex()
            .items_center()
            .h(px(HEADER))
            .pl(px(4.))
            .gap(px(2.))
            .child(ui::icon("confdeck", 18., t.fg).mr(px(6.)))
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("confdeck"),
            )
            .child(div().flex_1())
            .child(ui::icon_button("refresh", t, "rotate-cw"))
            .child(
                ui::icon_button("open-main", t, "square-arrow-out-up-right").on_click(cx.listener(
                    move |_, _, window, cx| {
                        close_for_main(window, cx);
                        main_window::show(store.clone(), cx);
                    },
                )),
            )
            .child(ui::icon_button("settings", t, "settings"))
    }

    fn agent_row(
        &self,
        i: usize,
        n: usize,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let agent = &store.agents[i];
        let open = self.picker.as_ref().is_some_and(|p| p.agent == i);
        let (logos, label) = store.pill(i);
        let readings = store.readings.read(cx);
        let reading = selection(store, readings, i);

        ui::row(t, true, n == 0)
            .h(px(row_height(store, readings, i)))
            .child(ui::logo(agent.logo, 18., t, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap(px(4.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(agent.name),
                    )
                    .children(
                        reading.map(|reading| quota_view::line(t, &format!("panel-{i}"), &reading)),
                    ),
            )
            // 宽度随内容，靠右对齐；名字太长时截断（magpie 面板里的下拉框也是这样）。
            .child(
                div().flex_none().max_w(px(230.)).child(
                    ui::select_pill(
                        ElementId::named_usize("pick", i),
                        t,
                        true,
                        &logos,
                        label,
                        open,
                        store.pill_dashed(i),
                        window,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.just_closed == Some(i) {
                            return;
                        }
                        if this.picker.as_ref().is_some_and(|p| p.agent == i) {
                            this.close_picker(window, cx);
                        } else {
                            this.open_picker(i, n, window, cx);
                        }
                    })),
                ),
            )
    }
}

impl Panel {
    /// 底栏左边：最近一次写文件的结果，一行放不下就截断，悬停看全文。
    fn status_line(&self, t: &Theme) -> Div {
        let slot = div().flex_1().min_w_0().h(px(LINKS));
        let Some(notice) = &self.status else {
            return slot;
        };
        let (icon, color) = match notice.kind {
            NoticeKind::Success => ("circle-check", t.success),
            NoticeKind::Error => ("circle-alert", t.danger),
        };
        let text_color = match notice.kind {
            NoticeKind::Success => t.muted,
            NoticeKind::Error => t.danger,
        };
        slot.child(
            div()
                .id("status")
                .h_flex()
                .items_center()
                .gap(px(5.))
                .pl(px(4.))
                .h_full()
                .text_size(px(11.5))
                .text_color(text_color)
                .child(ui::icon(icon, 12., color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(notice.text.clone()),
                )
                .tooltip(ui::tooltip(notice.text.clone())),
        )
    }
}

/// 关掉面板：只藏起来，不销毁窗口（`mac::hide` 说了为什么）。失焦回调里也会调到，
/// 所以真正 `orderOut` 放在下一轮主线程任务（notes/panel-focus-crash.md）。
/// 从后台打开的（Esc、再点托盘图标关掉时）把前台还给原来的应用，不然 AppKit 把焦点交给主窗口、把它提到前面。
fn close(window: &mut Window, cx: &mut App) {
    let state = cx.default_global::<PanelState>();
    if !state.shown {
        return;
    }
    state.shown = false;
    state.closed_at = Some(Instant::now());
    #[cfg(target_os = "macos")]
    let return_to = state.return_to.take();
    #[cfg(target_os = "macos")]
    trace!("close: hand back={}", return_to.is_some());
    window.blur(cx);
    #[cfg(target_os = "macos")]
    match return_to {
        Some(app) => crate::mac::hide_and_hand_back(window, app, cx),
        None => crate::mac::hide(window, cx),
    }
}

/// 关掉面板、接着打开主窗口：不把前台还回去。
fn close_for_main(window: &mut Window, cx: &mut App) {
    cx.default_global::<PanelState>().return_to = Default::default();
    close(window, cx);
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::panel(window.appearance());
        let store = self.store.read(cx);
        let shown = shown_agents(store);

        let mut list = ui::list(&t, true);
        for (n, &i) in shown.iter().enumerate() {
            list = list.child(self.agent_row(i, n, store, &t, window, cx));
        }
        let list = (!shown.is_empty()).then_some(list);
        let open_store = self.store.clone();

        // 选择器盖在列表上，从打开它的那一行下面展开，宽度和面板一样（左右各留 8）。
        // 不用 `anchored`：窗口比选择器矮时它会把选择器往上挪；这里固定位置，矮了就先截掉下面一截。
        let tall_enough = window.viewport_size().height >= px(self.height - 0.5);
        if let Some(open) = self.picker.as_mut() {
            if !open.drawn && tall_enough {
                trace!("picker: drawn");
            }
            open.drawn |= tall_enough;
        }
        let picker = self.picker.as_ref().filter(|open| open.drawn).map(|open| {
            deferred(
                div()
                    .absolute()
                    .top(px(row_bottom(store, store.readings.read(cx), open.row) + 4.))
                    .left(px(8.))
                    .w(px(WIDTH - 16.))
                    .on_mouse_down_out(cx.listener(move |this, _, window, cx| {
                        this.just_closed = this.picker.as_ref().map(|p| p.agent);
                        this.close_picker(window, cx);
                    }))
                    .child(open.picker.clone()),
            )
            .with_priority(1)
        });

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("Panel")
            .on_action(cx.listener(|_, _: &Dismiss, window, cx| close(window, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = None),
            )
            .rounded(px(12.))
            // 半透明的底叠在毛玻璃上（magpie 是 93% 的 --bg）。
            .bg(t.bg.opacity(0.93))
            .text_color(t.fg)
            .text_size(px(13.))
            .child(
                div()
                    .v_flex()
                    .gap(px(GAP))
                    .p(px(PADDING))
                    .child(self.header(&t, cx))
                    .children(list)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap(px(16.))
                            .pr(px(4.))
                            .child(self.status_line(&t))
                            .child(
                                ui::text_button("open", &t)
                                    .child(t!("panel.open"))
                                    .on_click(move |_, window, cx| {
                                        close_for_main(window, cx);
                                        main_window::show(open_store.clone(), cx);
                                    }),
                            )
                            .child(
                                ui::text_button("quit", &t)
                                    .child(t!("panel.quit"))
                                    .on_click(|_, _, cx| cx.quit()),
                            ),
                    ),
            )
            .children(picker)
    }
}
