//! 「配置 ›」页：一个 agent 用当前这家供应商时怎么写。顶上是面包屑、agent、文件路径和供应商胶囊
//! （和 Agent 页同一个选择器），下面每块一张卡：模型（Claude Code 的槽位；Codex、Grok Build 的列表加默认；
//! Pi、OpenCode 勾了几家就几块）、原始文件（默认收着，点开是编辑框）。
//!
//! 模型改一项写一次（`Store::save_override`：存进这家对这个 agent 的覆盖，再按覆盖重写文件），
//! 原始文件有「保存」。模型旁不标来源，下拉框里就是文件里的值；不做 diff。
//! 一次性的开关（跳过首次确认、VS Code 插件）不在这里，在设置页（decisions「配置页」）。

use std::time::Duration;

use confdeck_core::agent::{self, AgentKind, FileSetup, Mode, claude, slots};
use confdeck_core::engine::{LiveFile, Snapshot};
use confdeck_core::patch::{self, Format};
use confdeck_core::provider::{AgentOverride, Model, Protocol, Provider};
use gpui_kit::component::input::{
    Editor as CodeEditor, EditorState, Input, InputEvent, InputState,
};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Selection, Store};
use crate::picker::{Picker, PickerEvent};
use crate::theme::Theme;
use crate::ui::{self, Button};

pub enum ConfigEvent {
    /// 面包屑「‹ Agent」。
    Back,
    /// 选择器里的「把当前配置存成供应商…」：交给主窗口开预先填好的新建表单。
    SaveAs(Provider),
}

struct OpenPicker {
    picker: Entity<Picker>,
    _subscription: Subscription,
}

/// 刚改过、文件还没写完的值。文件写完重读之前，下拉框先显示它，不然会退回旧值闪一下。
enum Pending {
    /// `None` 是「留空」：不写这个槽位。
    Slot(&'static str, Option<String>),
    Default(String),
}

/// 点开的「原始文件」。
struct RawEditor {
    file: LiveFile,
    /// 打开时读到的内容，保存时核对文件没被改过。
    snapshot: Snapshot,
    state: Entity<EditorState>,
    rows: usize,
    error: Option<SharedString>,
}

pub struct ConfigPage {
    store: Entity<Store>,
    agent: usize,
    /// 单选 agent 的文件里现在写的模型；Store 变了就重读。
    setup: Option<FileSetup>,
    picker: Option<OpenPicker>,
    /// 刚被「点外面」关掉的选择器或下拉框。点的正好是它的按钮时，不要立刻再打开。
    just_closed: bool,
    /// 打开着的槽位下拉框（槽位名）。
    menu: Option<&'static str>,
    /// 下拉框底下手填模型的输入框。
    custom: Entity<InputState>,
    /// 列表卡、多选 agent 每块底下手填模型的输入框。
    extra: Entity<InputState>,
    /// 多选 agent：展开的是哪家（供应商下标）。
    expanded: Option<usize>,
    pending: Option<Pending>,
    raw: Option<RawEditor>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConfigEvent> for ConfigPage {}

impl ConfigPage {
    pub fn new(
        store: Entity<Store>,
        agent: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = Theme::window(window.appearance());
        crate::theme::sync_component_theme(&t, cx);
        let placeholder: SharedString = t!("config.other_model").into();
        let custom = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.clone()));
        let extra = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));

        let mut subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                this.refresh(cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &custom,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.add_custom(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&extra, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_extra(window, cx);
                }
            }),
        ];
        // Esc 关下拉框。输入框自己也绑了 Esc，在它之前拦下来；选择器开着时交给选择器。
        let page = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        subscriptions.push(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id {
                return;
            }
            let Some(page) = page.upgrade() else {
                return;
            };
            let keystroke = &event.keystroke;
            let modifiers = keystroke.modifiers;
            // ⌥⇧F：格式化打开着的原始文件（和 VS Code 一样的键）。
            if keystroke.key == "f"
                && modifiers.alt
                && modifiers.shift
                && !modifiers.platform
                && !modifiers.control
            {
                if page.read(cx).raw.is_some() {
                    page.update(cx, |page, cx| page.format_raw(window, cx));
                    cx.stop_propagation();
                }
                return;
            }
            if modifiers.modified() || keystroke.key != "escape" {
                return;
            }
            if page.read(cx).menu.is_some() {
                page.update(cx, |page, cx| {
                    page.menu = None;
                    cx.notify();
                });
                cx.stop_propagation();
            }
        }));

        let setup = store.read(cx).setup(agent);
        Self {
            store,
            agent,
            setup,
            picker: None,
            just_closed: false,
            menu: None,
            custom,
            extra,
            expanded: None,
            pending: None,
            raw: None,
            _subscriptions: subscriptions,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.setup = self.store.read(cx).setup(self.agent);
    }

    fn kind(&self, store: &Store) -> Option<AgentKind> {
        store.agents[self.agent].kind
    }

    /// 单选 agent 现在用的那家。
    fn single(&self, store: &Store) -> Option<usize> {
        match &store.agents[self.agent].selection {
            Selection::Single(Some(p)) => Some(*p),
            _ => None,
        }
    }

    /// 手填的输入框此刻对着哪家：单选是当前那家，多选是展开的那块。
    fn target(&self, store: &Store) -> Option<usize> {
        match store.agents[self.agent].mode {
            Mode::Switch => self.single(store),
            Mode::Coexist => self.expanded,
        }
    }

    /// 存改好覆盖的那家：Store 先换内存里的，文件在后台写；写完清掉「刚改的」、按文件重读。
    fn save(&mut self, provider: Provider, cx: &mut Context<Self>) {
        let agent = self.agent;
        let task = self
            .store
            .update(cx, |store, cx| store.save_override(agent, provider, cx));
        cx.spawn(async move |this, cx| {
            task.await;
            this.update(cx, |this, cx| {
                this.pending = None;
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 槽位选了一个模型。目录里没有的先加进目录（decisions「模型」）。
    /// 给一个槽位选模型；`None` 是「留空」，覆盖里存空串，这个环境变量就不写（core 的 `agent::slots`）。
    fn set_slot(&mut self, key: &'static str, model: Option<String>, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let (Some(kind), Some(p)) = (self.kind(store), self.single(store)) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        if let Some(model) = &model {
            ensure_model(&mut provider, model);
        }
        provider
            .overrides
            .entry(kind)
            .or_default()
            .slots
            .insert(key.to_string(), model.clone().unwrap_or_default());
        self.pending = Some(Pending::Slot(key, model));
        self.menu = None;
        self.save(provider, cx);
    }

    /// 「恢复默认」：清掉这个 agent 对这家的槽位，回到目录的默认模型。
    fn reset_slots(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let (Some(kind), Some(p)) = (self.kind(store), self.single(store)) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        if let Some(o) = provider.overrides.get_mut(&kind) {
            o.slots.clear();
            if *o == AgentOverride::default() {
                provider.overrides.remove(&kind);
            }
        }
        self.save(provider, cx);
    }

    /// 列表型、多选 agent：勾上或取消一个模型。最后一个不能取消。
    fn toggle_model(&mut self, p: usize, id: &str, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(kind) = self.kind(store) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        let mut listed = listed(&provider, kind);
        if listed.iter().any(|m| m == id) {
            if listed.len() == 1 {
                return;
            }
            listed.retain(|m| m != id);
            // 取消的是默认模型：默认退回列表里排第一的。
            if let Some(o) = provider.overrides.get_mut(&kind)
                && o.default_model.as_deref() == Some(id)
            {
                o.default_model = None;
            }
        } else {
            listed.push(id.to_string());
        }
        set_list(&mut provider, kind, listed);
        self.save(provider, cx);
    }

    /// 列表型 agent 的默认模型。不在列表里的先勾上。
    fn set_default(&mut self, p: usize, id: &str, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(kind) = self.kind(store) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        let mut listed = listed(&provider, kind);
        if !listed.iter().any(|m| m == id) {
            listed.push(id.to_string());
            set_list(&mut provider, kind, listed);
        }
        provider.overrides.entry(kind).or_default().default_model = Some(id.to_string());
        self.pending = Some(Pending::Default(id.to_string()));
        self.save(provider, cx);
    }

    /// 多选 agent：这家走哪种接口。
    fn set_protocol(&mut self, p: usize, protocol: Protocol, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(kind) = self.kind(store) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        provider.overrides.entry(kind).or_default().protocol = Some(protocol);
        self.save(provider, cx);
    }

    /// 槽位下拉框底下回车：手填的模型给这个槽位。
    fn add_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.menu else {
            return;
        };
        let value = self.custom.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        self.custom
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.set_slot(key, Some(value), cx);
    }

    /// 列表卡、多选块底下回车：手填的模型加进目录，也勾上。
    fn add_extra(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.extra.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let store = self.store.read(cx);
        let (Some(kind), Some(p)) = (self.kind(store), self.target(store)) else {
            return;
        };
        let mut provider = store.providers[p].clone();
        ensure_model(&mut provider, &value);
        let mut listed = listed(&provider, kind);
        if !listed.contains(&value) {
            listed.push(value);
            set_list(&mut provider, kind, listed);
        }
        self.extra
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.save(provider, cx);
    }

    fn toggle_menu(&mut self, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.just_closed {
            return;
        }
        if self.menu == Some(key) {
            self.menu = None;
        } else {
            self.menu = Some(key);
            self.picker = None;
            self.custom.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let agent = self.agent;
        let picker = cx.new(|cx| Picker::new(store, agent, false, window, cx));
        let _subscription = cx.subscribe_in(
            &picker,
            window,
            move |this, _, event: &PickerEvent, _, cx| {
                match event {
                    PickerEvent::Picked(option) => {
                        let option = *option;
                        this.store
                            .update(cx, |s, cx| s.switch(agent, option, cx))
                            .detach();
                    }
                    PickerEvent::Toggled(provider) => {
                        let provider = *provider;
                        this.store
                            .update(cx, |s, cx| s.toggle(agent, provider, cx))
                            .detach();
                        return;
                    }
                    PickerEvent::AdoptKey { provider, key } => {
                        let (provider, key) = (*provider, key.clone());
                        this.store
                            .update(cx, |s, cx| s.adopt_key(agent, provider, key, cx))
                            .detach();
                    }
                    PickerEvent::SaveAs(draft) => cx.emit(ConfigEvent::SaveAs(draft.clone())),
                    PickerEvent::Dismissed => {}
                }
                this.picker = None;
                cx.notify();
            },
        );
        self.picker = Some(OpenPicker {
            picker,
            _subscription,
        });
        self.menu = None;
        cx.notify();
    }

    /// 「原始文件」点「打开」：读一遍文件，整份放进编辑框。
    fn open_raw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(file) = store.live_file(self.agent) else {
            return;
        };
        let (snapshot, error) = match store.read_file(&file) {
            Ok(snapshot) => (snapshot, None),
            Err(err) => (
                Snapshot {
                    bytes: None,
                    digest: None,
                },
                Some(err.to_string().into()),
            ),
        };
        let text = snapshot
            .bytes
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        let rows = text.lines().count().max(1);
        let language = match &file.format {
            Format::Json | Format::Json5 => "json",
            Format::Toml => "toml",
        };
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .default_value(text)
        });
        state.update(cx, |state, cx| state.focus(window, cx));
        self.raw = Some(RawEditor {
            file,
            snapshot,
            state,
            rows,
            error,
        });
        self.menu = None;
        cx.notify();
    }

    fn save_raw(&mut self, cx: &mut Context<Self>) {
        let Some(raw) = &self.raw else {
            return;
        };
        let bytes = raw.state.read(cx).value().to_string().into_bytes();
        let (file, snapshot) = (raw.file.clone(), raw.snapshot.clone());
        let result = self.store.update(cx, |store, cx| {
            store.replace_file(&file, &bytes, &snapshot, cx)
        });
        match result {
            Ok(_) => self.raw = None,
            Err(err) => {
                if let Some(raw) = &mut self.raw {
                    raw.error = Some(err.to_string().into());
                }
            }
        }
        cx.notify();
    }

    /// 「格式化」（按钮或 ⌥⇧F）：重新排版编辑框里的内容，不写文件；解析不了把错误显示在底栏。
    fn format_raw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(raw) = &mut self.raw else {
            return;
        };
        let text = raw.state.read(cx).value().to_string();
        let Some(result) = patch::format(raw.file.format, &raw.file.path, text.as_bytes()) else {
            return;
        };
        match result {
            Ok(bytes) => {
                let formatted = String::from_utf8_lossy(&bytes).into_owned();
                raw.rows = formatted.lines().count().max(1);
                raw.error = None;
                if formatted != text {
                    raw.state
                        .update(cx, |state, cx| state.set_value(formatted, window, cx));
                }
            }
            Err(err) => raw.error = Some(err.to_string().into()),
        }
        cx.notify();
    }

    /// 这个槽位显示什么：刚改的 > 文件里的（没有这个键就是留空）> 文件读不到时按目录算的。
    fn slot_value(
        &self,
        provider: &Provider,
        kind: AgentKind,
        key: &'static str,
    ) -> Option<String> {
        if let Some(Pending::Slot(pending, value)) = &self.pending
            && *pending == key
        {
            return value.clone();
        }
        // 文件读得到就以文件为准：没有这个键就是留空，不拿目录算的顶上。
        if let Some(setup) = &self.setup {
            return setup.slots.get(key).cloned();
        }
        let all: Vec<slots::Slot> = claude::SLOTS.iter().map(|(slot, _)| *slot).collect();
        let auto = !provider.is_anthropic_official();
        slots::fill(provider, kind, &all, auto)
            .ok()?
            .into_iter()
            .find(|(slot, _)| slot.key == key)
            .map(|(_, id)| id.to_string())
    }

    /// 列表型 agent 的默认模型显示什么：刚改的 > 文件里的（还在目录里时）> 按覆盖算出来的。
    fn default_value(&self, provider: &Provider, kind: AgentKind) -> Option<String> {
        if let Some(Pending::Default(value)) = &self.pending {
            return Some(value.clone());
        }
        if let Some(value) = self
            .setup
            .as_ref()
            .and_then(|setup| setup.default_model.clone())
            .filter(|id| provider.model(id).is_some())
        {
            return Some(value);
        }
        agent::default_model(provider, kind)
            .ok()
            .map(|model| model.id.clone())
    }

    fn crumb(&self, t: &Theme, cx: &Context<Self>) -> Div {
        div().h_flex().flex_none().items_center().h(px(24.)).child(
            ui::text_button("back", t)
                .gap(px(2.))
                .child(ui::icon("chevron-left", 13., t.ctl_fg))
                .child(t!("tabs.agents"))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ConfigEvent::Back))),
        )
    }

    fn header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let agent = &store.agents[self.agent];
        let (logos, label) = store.pill(self.agent);
        let open = self.picker.as_ref();
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .px(px(4.))
            .pb(px(2.))
            .child(ui::logo(agent.logo, 28., t, window, cx))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(agent.name),
                    )
                    .child(
                        div()
                            .mt(px(1.))
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .truncate()
                            .child(agent.config_path()),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(224.))
                    .child(
                        ui::select_pill(
                            "pick",
                            t,
                            false,
                            &logos,
                            label,
                            open.is_some(),
                            store.pill_dashed(self.agent),
                            window,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.just_closed {
                                return;
                            }
                            if this.picker.is_some() {
                                this.picker = None;
                                cx.notify();
                            } else {
                                this.open_picker(window, cx);
                            }
                        })),
                    )
                    .when_some(open, |d, open| {
                        d.child(ui::dropdown(
                            div()
                                .w(px(320.))
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.picker = None;
                                    this.just_closed = true;
                                    cx.notify();
                                }))
                                .child(open.picker.clone()),
                        ))
                    }),
            )
    }

    /// 「模型」卡。选「默认」（官方登录）、自定义、Key 不一致，或多选 agent 一家都没勾时不显示。
    fn models_card(&self, t: &Theme, window: &mut Window, cx: &Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        let kind = self.kind(store)?;
        match &store.agents[self.agent].selection {
            Selection::Single(Some(p)) => Some(match kind {
                AgentKind::ClaudeCode => self.slots_card(*p, kind, store, t, cx),
                _ => self.list_card(*p, kind, store, t, window, cx),
            }),
            Selection::Single(None) => None,
            Selection::Multi(enabled) if enabled.is_empty() => None,
            Selection::Multi(enabled) => Some(self.multi_card(enabled, kind, store, t, window, cx)),
        }
    }

    /// 槽位型（Claude Code）：每行一个槽位，右边从这家的目录里挑。小字是写进文件的键名。
    fn slots_card(
        &self,
        p: usize,
        kind: AgentKind,
        store: &Store,
        t: &Theme,
        cx: &Context<Self>,
    ) -> Div {
        let provider = &store.providers[p];
        let edited = provider
            .override_for(kind)
            .is_some_and(|o| !o.slots.is_empty());
        let mut card = ui::list(t, false).child(
            ui::card_title(t, t!("config.models"))
                .child(div().flex_1())
                .when(edited, |row| {
                    row.child(
                        ui::text_button("reset-slots", t)
                            .font_weight(FontWeight::NORMAL)
                            .child(t!("config.reset_slots"))
                            .on_click(cx.listener(|this, _, _, cx| this.reset_slots(cx))),
                    )
                }),
        );
        for (n, (slot, env)) in claude::SLOTS.iter().enumerate() {
            let key = slot.key;
            let value = self.slot_value(provider, kind, key);
            let open = self.menu == Some(key);
            let shown: SharedString = value.clone().unwrap_or_else(|| "—".into()).into();
            card = card.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(40.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .child(
                        div()
                            .flex_none()
                            .w(px(80.))
                            .text_size(px(12.5))
                            .child(slot_label(key)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family("Menlo")
                            .text_size(px(10.5))
                            .text_color(t.faint)
                            .child(*env),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .w(px(260.))
                            .child(
                                ui::menu_pill(ElementId::named_usize("slot", n), t, shown, open)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toggle_menu(key, window, cx)
                                    })),
                            )
                            .when(open, |d| {
                                d.child(ui::dropdown(self.model_menu(
                                    provider,
                                    value.as_deref(),
                                    key,
                                    t,
                                    cx,
                                )))
                            }),
                    ),
            );
        }
        card
    }

    /// 槽位的下拉框：这家的目录，底下一行手填。
    fn model_menu(
        &self,
        provider: &Provider,
        current: Option<&str>,
        key: &'static str,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let hover = t.sel;
        // 第一项「留空」：不写这个环境变量，agent 用自己的默认。
        let mut list = div()
            .id("model-menu")
            .v_flex()
            .p(px(5.))
            .max_h(px(260.))
            .overflow_y_scroll()
            .child(
                div()
                    .id("model-empty")
                    .h_flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(32.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_slot(key, None, cx)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(t.muted)
                            .child(t!("config.leave_empty")),
                    )
                    .when(current.is_none(), |row| {
                        row.child(ui::icon("check", 13., t.accent))
                    }),
            );
        for (n, model) in provider.models.iter().enumerate() {
            let id = model.id.clone();
            let on = current == Some(model.id.as_str());
            list = list.child(
                div()
                    .id(ElementId::named_usize("model-option", n))
                    .h_flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(32.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_slot(key, Some(id.clone()), cx)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family("Menlo")
                            .text_size(px(12.))
                            .child(model.id.clone()),
                    )
                    .when_some(model.name.clone(), |row, name| {
                        row.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(t.muted)
                                .child(name),
                        )
                    })
                    .child(div().flex_1())
                    .when(on, |row| row.child(ui::icon("check", 13., t.accent))),
            );
        }
        div()
            .v_flex()
            .w(px(300.))
            .bg(Hsla { a: 1., ..t.pop_bg })
            .rounded(px(11.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .overflow_hidden()
            .occlude()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                this.just_closed = true;
                cx.notify();
            }))
            .child(list)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .px(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .child(ui::icon("plus", 13., t.faint))
                    .child(
                        div()
                            .flex_1()
                            .font_family("Menlo")
                            .text_size(px(12.))
                            .child(
                                Input::new(&self.custom)
                                    .appearance(false)
                                    .xsmall()
                                    .text_size(px(12.)),
                            ),
                    ),
            )
            .with_animation(
                "model-menu-in",
                Animation::new(Duration::from_millis(320)).with_easing(ui::ease_out_quart),
                |el, d| el.opacity((d * 2.2).min(1.)).mt(px(-6. * (1. - d))),
            )
    }

    /// 列表加默认（Codex、Grok Build）：勾选是写不写进去，右边的点是默认。
    fn list_card(
        &self,
        p: usize,
        kind: AgentKind,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let provider = &store.providers[p];
        let listed = listed(provider, kind);
        let default = self.default_value(provider, kind);
        let mut card = ui::list(t, false).child(ui::card_title(t, t!("config.models")));
        for (n, model) in provider.models.iter().enumerate() {
            let on = listed.contains(&model.id);
            let is_default = default.as_deref() == Some(model.id.as_str());
            card = card.child(
                self.model_row(p, n, model, on, Some(is_default), t, cx)
                    .border_t_1()
                    .border_color(t.line_2),
            );
        }
        card.child(self.extra_row(t, 14., window, cx))
    }

    /// 多选 agent（Pi、OpenCode）：勾了几家就几块，点开看这家写哪些模型、走哪种接口。
    fn multi_card(
        &self,
        enabled: &[usize],
        kind: AgentKind,
        store: &Store,
        t: &Theme,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Div {
        let mut card = ui::list(t, false).child(ui::card_title(t, t!("config.models")));
        for &p in enabled {
            let provider = &store.providers[p];
            let expanded = self.expanded == Some(p);
            let listed = listed(provider, kind);
            let protocol = agent::choose_protocol(provider, kind)
                .ok()
                .map(|(protocol, _)| protocol);
            let count = if listed.len() == 1 {
                t!("config.count_one")
            } else {
                t!("config.count_other", count = listed.len())
            };
            let sub: SharedString = match protocol {
                Some(protocol) => format!("{count} · {protocol}").into(),
                None => count.to_string().into(),
            };
            card = card.child(
                div()
                    .id(ElementId::named_usize("block", p))
                    .h_flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(44.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.expanded = if this.expanded == Some(p) {
                            None
                        } else {
                            Some(p)
                        };
                        cx.notify();
                    }))
                    .child(ui::logo(data::logo(provider), 18., t, window, cx))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(provider.name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .child(sub),
                    )
                    .child(div().flex_1())
                    .child(ui::icon(
                        if expanded {
                            "chevron-up"
                        } else {
                            "chevron-down"
                        },
                        12.,
                        t.faint,
                    )),
            );
            if !expanded {
                continue;
            }
            // 接口：这家提供、这个 agent 也能用的不止一种时才给选。
            let choices: Vec<(Protocol, SharedString)> = kind
                .protocols()
                .iter()
                .copied()
                .filter(|protocol| provider.endpoints.contains_key(protocol))
                .map(|protocol| (protocol, protocol.to_string().into()))
                .collect();
            if choices.len() > 1
                && let Some(active) = protocol
            {
                card = card.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap(px(12.))
                        .h(px(40.))
                        .pl(px(42.))
                        .pr(px(12.))
                        .child(
                            div()
                                .flex_none()
                                .w(px(80.))
                                .text_size(px(12.5))
                                .text_color(t.fg_2)
                                .child(t!("config.protocol")),
                        )
                        .child(ui::seg(
                            t,
                            &choices,
                            active,
                            None,
                            window,
                            cx.listener(move |this, protocol: &Protocol, _, cx| {
                                this.set_protocol(p, *protocol, cx)
                            }),
                        )),
                );
            }
            for (n, model) in provider.models.iter().enumerate() {
                let on = listed.contains(&model.id);
                card = card.child(self.model_row(p, n, model, on, None, t, cx).pl(px(42.)));
            }
            card = card.child(self.extra_row(t, 42., window, cx));
        }
        card
    }

    /// 列表里的一行：勾选框、模型名、显示名；`default` 是 `Some` 时右边多一个默认的点。
    #[allow(clippy::too_many_arguments)]
    fn model_row(
        &self,
        p: usize,
        n: usize,
        model: &Model,
        on: bool,
        default: Option<bool>,
        t: &Theme,
        cx: &Context<Self>,
    ) -> Div {
        let id = model.id.clone();
        let id_for_default = model.id.clone();
        div()
            .h_flex()
            .items_center()
            .gap(px(10.))
            .h(px(40.))
            .pl(px(14.))
            .pr(px(12.))
            .child(
                div()
                    .id(ElementId::named_usize("member", n))
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_model(p, &id, cx)))
                    .child(ui::checkbox(t, on))
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_size(px(12.))
                            .when(!on, |d| d.text_color(t.muted))
                            .child(model.id.clone()),
                    ),
            )
            .when_some(model.name.clone(), |row, name| {
                row.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(t.muted)
                        .child(name),
                )
            })
            .child(div().flex_1())
            .when_some(default, |row, is_default| {
                row.child(
                    div()
                        .id(ElementId::named_usize("default", n))
                        .flex_none()
                        .h_flex()
                        .items_center()
                        .justify_end()
                        .gap(px(6.))
                        .min_w(px(60.))
                        .cursor_pointer()
                        .tooltip(ui::tooltip(t!("editor.default_model")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_default(p, &id_for_default, cx)
                        }))
                        .when(is_default, |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(t.muted)
                                    .child(t!("common.default")),
                            )
                        })
                        .child(ui::radio(t, is_default)),
                )
            })
    }

    /// 卡片底下手填模型的一行：加号和一个没有边框的输入框，回车加进目录。
    fn extra_row(&self, t: &Theme, indent: f32, window: &Window, cx: &Context<Self>) -> Div {
        let focused = self.extra.read(cx).focus_handle(cx).is_focused(window);
        div()
            .h_flex()
            .items_center()
            .gap(px(10.))
            .h(px(40.))
            .pl(px(indent))
            .pr(px(12.))
            .border_t_1()
            .border_color(t.line_2)
            .child(ui::icon(
                "plus",
                13.,
                if focused { t.muted } else { t.faint },
            ))
            .child(
                div()
                    .flex_1()
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .child(
                        Input::new(&self.extra)
                            .appearance(false)
                            .xsmall()
                            .text_size(px(12.)),
                    ),
            )
    }

    /// 「原始文件」卡：收着时一行（标题、路径、「打开」），点开是编辑框和取消 / 保存。
    fn raw_card(&self, t: &Theme, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let path: SharedString = store
            .live_file(self.agent)
            .map(|file| store.tilde(&file.path))
            .unwrap_or_default()
            .into();
        let title = ui::card_title(t, t!("config.raw_file"))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::NORMAL)
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(path),
            )
            .child(div().flex_1());
        let Some(raw) = &self.raw else {
            return ui::list(t, false).child(
                title.child(
                    ui::text_button("raw-open", t)
                        .font_weight(FontWeight::NORMAL)
                        .child(t!("config.open"))
                        .child(ui::icon("chevron-down", 11., t.faint))
                        .on_click(cx.listener(|this, _, window, cx| this.open_raw(window, cx))),
                ),
            );
        };
        let height = (raw.rows as f32 * 20. + 16.).clamp(120., 380.);
        let error = raw.error.clone();
        let formattable = raw.file.format != Format::Toml;
        ui::list(t, false)
            .child(title)
            .child(
                div()
                    .flex_none()
                    .mx(px(14.))
                    .rounded(px(8.))
                    .bg(t.field)
                    .border_1()
                    .border_color(t.line)
                    .overflow_hidden()
                    .child(
                        CodeEditor::new(&raw.state)
                            .appearance(false)
                            .bordered(false)
                            .h(px(height))
                            .text_size(px(12.)),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(54.))
                    .px(px(14.))
                    .when(formattable, |row| {
                        row.child(
                            ui::text_button("raw-format", t)
                                .tooltip(ui::tooltip("⌥⇧F"))
                                .child(t!("config.format"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.format_raw(window, cx)),
                                ),
                        )
                    })
                    .child(match error {
                        Some(error) => div()
                            .flex_1()
                            .min_w_0()
                            .h_flex()
                            .items_center()
                            .gap(px(5.))
                            .text_size(px(11.5))
                            .text_color(t.danger)
                            .child(ui::icon("circle-alert", 13., t.danger))
                            .child(div().min_w_0().truncate().child(error)),
                        None => div().flex_1(),
                    })
                    .child(
                        ui::button("raw-cancel", t, Button::Secondary)
                            .child(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.raw = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::button("raw-save", t, Button::Primary)
                            .child(t!("common.save"))
                            .on_click(cx.listener(|this, _, _, cx| this.save_raw(cx))),
                    ),
            )
    }
}

impl Render for ConfigPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let models = self.models_card(&t, window, cx);
        div()
            .v_flex()
            .gap(px(10.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = false),
            )
            .child(self.crumb(&t, cx))
            .child(self.header(&t, window, cx))
            .children(models)
            .child(self.raw_card(&t, cx))
    }
}

/// 槽位的名字（core 的 `claude::SLOTS`）。
fn slot_label(key: &str) -> SharedString {
    match key {
        "main" => t!("config.slot_main").into(),
        "opus" => t!("config.slot_opus").into(),
        "sonnet" => t!("config.slot_sonnet").into(),
        "fable" => t!("config.slot_fable").into(),
        "haiku" => t!("config.slot_haiku").into(),
        "subagent" => t!("config.slot_subagent").into(),
        other => other.to_string().into(),
    }
}

/// 列表型、多选 agent 现在写哪些模型（覆盖的，或整个目录）。
fn listed(provider: &Provider, kind: AgentKind) -> Vec<String> {
    agent::list_models(provider, kind)
        .map(|models| models.iter().map(|model| model.id.clone()).collect())
        .unwrap_or_default()
}

/// 目录里没有的模型加进目录（decisions「模型」：手填的加进目录）。
fn ensure_model(provider: &mut Provider, id: &str) {
    if provider.model(id).is_none() {
        provider.models.push(Model {
            id: id.to_string(),
            ..Model::default()
        });
    }
}

/// 列表存进覆盖：和整个目录一样就不存，不然按目录的顺序存子集。覆盖空了就删掉。
fn set_list(provider: &mut Provider, kind: AgentKind, listed: Vec<String>) {
    let ordered: Vec<String> = provider
        .models
        .iter()
        .map(|model| model.id.clone())
        .filter(|id| listed.contains(id))
        .collect();
    let whole = ordered.len() == provider.models.len();
    let o = provider.overrides.entry(kind).or_default();
    o.models = if whole { None } else { Some(ordered) };
    if *o == AgentOverride::default() {
        provider.overrides.remove(&kind);
    }
}

#[cfg(test)]
mod tests {
    use confdeck_core::agent::claude;

    use super::slot_label;

    /// 槽位名是按 core 的清单翻的，清单多了一个这里就会原样显示键名。
    #[test]
    fn every_claude_slot_has_a_label() {
        for (slot, _) in claude::SLOTS {
            assert_ne!(slot_label(slot.key), slot.key, "{} has no label", slot.key);
        }
    }
}
