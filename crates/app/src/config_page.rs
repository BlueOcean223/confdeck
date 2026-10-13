//! 「配置 ›」页：一个 agent 用当前这家供应商时怎么写。顶上是面包屑、agent、文件路径和供应商胶囊
//! （和 Agent 页同一个选择器），下面每块一张卡：模型（Claude Code 的槽位；Codex、Grok Build 的列表加默认；
//! Pi、OpenCode 勾了几家就几块）、原始文件（一直开着的编辑框）。
//!
//! 改了不马上写（用户 10-12，效法 cc-switch）：模型的改动记成草稿（`drafts`，改好覆盖的那家），
//! 编辑框跟着显示保存后的样子，手改编辑框也算改动。有改动时底下浮出一条「保存 / 取消」（主窗口画，`save_bar`）：
//! 保存才按覆盖写文件（`Store::save_override`），手改过的再整份写进去；取消全部丢掉、重读文件。
//! 有改动时不让离开这页、不让换供应商，底栏晃一下。
//! 模型旁不标来源，下拉框里就是编辑框里的值；不做 diff。
//! 一次性的开关（跳过首次确认、VS Code 插件）不在这里，在设置页（decisions「配置页」）。

use std::cell::Cell;
use std::f32::consts::PI;
use std::rc::Rc;
use std::time::Duration;

use confdeck_core::agent::{self, AgentKind, FileSetup, Mode, claude, slots};
use confdeck_core::engine::{EngineError, LiveFile, Snapshot};
use confdeck_core::patch::{self, Format, PatchError};
use confdeck_core::provider::{AgentOverride, Model, Protocol, Provider};
use confdeck_core::status::AgentStatus;
use gpui_kit::component::input::{
    Editor as CodeEditor, EditorState, Input, InputEvent, InputState,
};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Store};
use crate::picker::{Picker, PickerEvent};
use crate::theme::Theme;
use crate::ui::{self, Button};
use crate::write::Written;

pub enum ConfigEvent {
    /// 面包屑「‹ Agent」。
    Back,
    /// 选择器里的「把当前配置存成供应商…」：交给主窗口开预先填好的新建表单。
    SaveAs(Provider),
    /// 编辑框里的光标挪到了窗口里这一段（上、下边，窗口坐标）：编辑框不自己滚，由主窗口把页面滚过去。
    Reveal(Pixels, Pixels),
}

struct OpenPicker {
    picker: Entity<Picker>,
    _subscription: Subscription,
}

/// 「原始文件」的编辑框。
struct RawEditor {
    file: LiveFile,
    /// 读出来时的内容，保存手改的时核对文件没被别处改过。
    snapshot: Snapshot,
    /// 读出来的文本。
    loaded: String,
    /// 只按草稿改、没手改时编辑框该是什么样（只存覆盖时文件会写成的样子）。编辑框和它不一样就是手改过。
    expected: String,
    state: Entity<EditorState>,
    rows: usize,
    /// 编辑框上一帧在窗口里的位置，算光标那一行在哪。
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 上次看到的光标位置和文本长度，变了才让页面跟过去（光标闪烁也会通知）。
    caret: (usize, usize),
    /// 格式化失败、草稿补不进编辑框时的原因，显示在编辑框底下。
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
    /// 还没保存的模型改动：改好覆盖的供应商，一家一份。
    drafts: Vec<Provider>,
    /// 记草稿时这个 agent 在用哪几家（供应商 id）。别处（菜单栏面板）换了，草稿对不上了，丢掉。
    in_use: Vec<String>,
    /// 文件连的就是选中的那家（`Store::settled`）。不是时下拉框不拿文件里的值。
    settled: bool,
    /// 上次读文件时 Store 换上过几份快照（`Store::generation`）。没换过就不重读文件：Store 别的通知（额度查回来了这类）和文件无关。
    seen: u64,
    raw: Option<RawEditor>,
    saving: bool,
    /// 保存没成的原因，显示在底栏。
    failure: Option<SharedString>,
    /// 有改动时想离开或换供应商的次数：底栏晃一下，字变红。保存、取消后清零。
    nudges: u64,
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
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.refresh(window, cx);
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
            // ⌘S：保存。
            if keystroke.key == "s"
                && modifiers.platform
                && !modifiers.alt
                && !modifiers.shift
                && !modifiers.control
            {
                if page.read(cx).dirty(cx) {
                    page.update(cx, |page, cx| page.save(window, cx));
                    cx.stop_propagation();
                }
                return;
            }
            // ⌥⇧F：格式化原始文件（和 VS Code 一样的键），光标在编辑框里时。
            if keystroke.key == "f"
                && modifiers.alt
                && modifiers.shift
                && !modifiers.platform
                && !modifiers.control
            {
                let focused = page
                    .read(cx)
                    .raw
                    .as_ref()
                    .is_some_and(|raw| raw.state.focus_handle(cx).is_focused(window));
                if focused {
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

        let raw = Self::open_raw(&store, agent, window, cx);
        if let Some(raw) = &raw {
            subscriptions.push(cx.subscribe_in(
                &raw.state,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if let InputEvent::Change = event {
                        this.edited(cx);
                    }
                },
            ));
            subscriptions.push(cx.observe_in(&raw.state, window, |this, _, window, cx| {
                this.follow_caret(window, cx)
            }));
        }
        let mut page = Self {
            store,
            agent,
            setup: None,
            picker: None,
            just_closed: false,
            menu: None,
            custom,
            extra,
            expanded: None,
            drafts: Vec::new(),
            in_use: Vec::new(),
            settled: false,
            seen: 0,
            raw,
            saving: false,
            failure: None,
            nudges: 0,
            _subscriptions: subscriptions,
        };
        page.settled = page.store.read(cx).settled(agent);
        page.seen = page.store.read(cx).generation();
        page.reparse(cx);
        page
    }

    /// 读一遍文件，整份放进编辑框。
    fn open_raw(
        store: &Entity<Store>,
        agent: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<RawEditor> {
        let file = store.read(cx).live_file(agent)?;
        let (snapshot, text, error) = read(store.read(cx), &file);
        let language = match &file.format {
            Format::Json | Format::Json5 => "json",
            Format::Toml => "toml",
        };
        let rows = rows(&text);
        let state = cx.new(|cx| {
            // 编辑框跟着内容撑高、不自己滚（只留页面那一个滚动条）：不自动换行，行数才算得准；
            // 末行下面也不留空。
            EditorState::new(window, cx)
                .language(language)
                .soft_wrap(false)
                .scroll_beyond_last_line(Some(0))
                .default_value(text.clone())
        });
        Some(RawEditor {
            file,
            snapshot,
            loaded: text.clone(),
            expected: text,
            state,
            rows,
            bounds: Rc::default(),
            caret: (0, 0),
            error,
        })
    }

    /// 有没有还没保存的改动：模型的草稿，或者手改过编辑框。
    pub fn dirty(&self, cx: &App) -> bool {
        !self.drafts.is_empty() || self.hand_edited(cx)
    }

    fn hand_edited(&self, cx: &App) -> bool {
        self.raw
            .as_ref()
            .is_some_and(|raw| raw.state.read(cx).value().as_ref() != raw.expected)
    }

    /// 有改动时想离开（面包屑、切标签）或换供应商：不走，底栏晃一下。
    pub fn nudge(&mut self, cx: &mut Context<Self>) {
        self.nudges += 1;
        self.picker = None;
        self.menu = None;
        cx.notify();
    }

    /// Store 变了（这里或面板换了供应商、保存写完了）：草稿对不上的丢掉；文件变了、没手改就重读。
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let store = self.store.read(cx);
        let was_settled = std::mem::replace(&mut self.settled, store.settled(self.agent));
        if !self.drafts.is_empty() && in_use(store, self.agent) != self.in_use {
            self.drafts.clear();
        }
        let before = self.drafts.len();
        self.drafts.retain(|draft| {
            store
                .providers
                .iter()
                .any(|p| p.id == draft.id && p != draft)
        });
        let dropped = self.drafts.len() != before;
        if self.hand_edited(cx) {
            return;
        }
        let generation = store.generation();
        let stale = generation != self.seen;
        self.seen = generation;
        let Some(raw) = &mut self.raw else {
            return;
        };
        if stale {
            let (snapshot, text, error) = read(store, &raw.file);
            if snapshot.digest != raw.snapshot.digest {
                raw.snapshot = snapshot;
                raw.loaded = text;
                raw.error = error;
                self.sync(window, cx);
                return;
            }
        }
        // 编辑框没变、快照也没换：下拉框的值不会变，不再解析（Codex 解析时要读模型目录）。
        if dropped {
            self.sync(window, cx);
        } else if stale || was_settled != self.settled {
            self.reparse(cx);
        }
    }

    /// 编辑框换成「读出来的内容 + 草稿」：没手改时用。
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let expected = self.expected(cx);
        let Some(raw) = &mut self.raw else {
            return;
        };
        match expected {
            Ok(expected) => {
                raw.expected = expected.clone();
                set_text(raw, expected, window, cx);
            }
            Err(err) => raw.error = Some(err.into()),
        }
        self.reparse(cx);
    }

    /// 读出来的内容依次补上每份草稿：只存覆盖时文件会写成的样子。
    fn expected(&self, cx: &App) -> Result<String, String> {
        let Some(raw) = &self.raw else {
            return Ok(String::new());
        };
        let store = self.store.read(cx);
        self.drafts
            .iter()
            .try_fold(raw.loaded.clone(), |text, draft| {
                let Some(saved) = store.providers.iter().find(|p| p.id == draft.id) else {
                    return Ok(text);
                };
                store
                    .preview(self.agent, saved, draft, &text)
                    .map_err(|err| err.to_string())
            })
    }

    /// 编辑框里的内容写的模型，给下拉框用。手改到一半解析不了时留着上一次的。
    fn reparse(&mut self, cx: &App) {
        if !self.settled {
            self.setup = None;
            return;
        }
        let Some(raw) = &self.raw else {
            return;
        };
        let text = raw.state.read(cx).value();
        if let Ok(setup) = self.store.read(cx).setup_of(self.agent, &text) {
            self.setup = setup;
        }
    }

    /// 编辑框不自己滚，光标挪出窗口时让页面跟过去：算出光标那一行在窗口里的位置交给主窗口。
    fn follow_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(raw) = &mut self.raw else {
            return;
        };
        if !raw.state.focus_handle(cx).is_focused(window) {
            return;
        }
        let state = raw.state.read(cx);
        let text = state.value();
        let cursor = state.cursor();
        if raw.caret == (cursor, text.len()) {
            return;
        }
        raw.caret = (cursor, text.len());
        let Some(line) = state.line_height() else {
            return;
        };
        let row = text
            .get(..cursor)
            .map_or(0, |before| before.matches('\n').count());
        // 上内边距 8。
        let top = raw.bounds.get().top() + px(8.) + line * row as f32;
        cx.emit(ConfigEvent::Reveal(top, top + line));
    }

    /// 手改了编辑框。
    fn edited(&mut self, cx: &mut Context<Self>) {
        if let Some(raw) = &mut self.raw {
            raw.rows = rows(&raw.state.read(cx).value());
            raw.error = None;
        }
        self.failure = None;
        self.reparse(cx);
        cx.notify();
    }

    /// 改了一项模型设置：`provider` 是改好覆盖的那家。记成草稿，编辑框跟着改：
    /// 没手改就换成按草稿算的样子，手改过就只把这一下补进去，手改的留着。
    fn stage(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let store = self.store.read(cx);
        let Some(saved) = store.providers.iter().find(|p| p.id == provider.id) else {
            return;
        };
        let before = self
            .drafts
            .iter()
            .find(|draft| draft.id == provider.id)
            .unwrap_or(saved)
            .clone();
        if before == provider {
            return;
        }
        if self.drafts.is_empty() {
            self.in_use = in_use(store, self.agent);
        }
        let unchanged = provider == *saved;
        self.drafts.retain(|draft| draft.id != provider.id);
        if !unchanged {
            self.drafts.push(provider.clone());
        }
        self.failure = None;
        self.menu = None;
        if !self.hand_edited(cx) {
            self.sync(window, cx);
            cx.notify();
            return;
        }
        let expected = self.expected(cx);
        let Some(raw) = &mut self.raw else {
            return;
        };
        if let Ok(expected) = expected {
            raw.expected = expected;
        }
        let text = raw.state.read(cx).value().to_string();
        match store.preview(self.agent, &before, &provider, &text) {
            Ok(next) => {
                raw.error = None;
                set_text(raw, next, window, cx);
            }
            Err(err) => raw.error = Some(err.to_string().into()),
        }
        self.reparse(cx);
        cx.notify();
    }

    /// 底栏「保存」（⌘S）：先按草稿写覆盖和文件，手改过的再把编辑框整份写进去。
    /// 手改的解析不了、文件读出来以后被别处改过，就什么都不写，原因显示在底栏。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || !self.dirty(cx) {
            return;
        }
        let Some(raw) = &self.raw else {
            return;
        };
        let text = raw.state.read(cx).value().to_string();
        let hand_edited = text != raw.expected;
        let file = raw.file.clone();
        if hand_edited {
            let checked = patch::check(file.format, &file.path, text.as_bytes())
                .map_err(|err| describe(&err))
                .and_then(|()| match self.store.read(cx).read_file(&file) {
                    Ok(now) if now.digest == raw.snapshot.digest => Ok(()),
                    Ok(_) => Err(t!("config.conflict").into()),
                    Err(err) => Err(describe_write(&err)),
                });
            if let Err(err) = checked {
                self.failure = Some(err);
                cx.notify();
                return;
            }
        }
        self.saving = true;
        self.failure = None;
        self.menu = None;
        let drafts = std::mem::take(&mut self.drafts);
        // 同一次保存已经说过「Claude Code 已更新」，整份写进去时就不再说「已保存 settings.json」。
        let announce = drafts.is_empty();
        let agent = self.agent;
        let writes: Vec<Written> = drafts
            .into_iter()
            .map(|provider| {
                self.store
                    .update(cx, |store, cx| store.save_override(agent, provider, cx))
            })
            .collect();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            for write in writes {
                write.report().await;
            }
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                this.nudges = 0;
                let mut failed = None;
                if hand_edited {
                    let result = this.store.update(cx, |store, cx| {
                        let now = store.read_file(&file)?;
                        store.replace_file(&file, text.as_bytes(), &now, announce, cx)
                    });
                    failed = result.err();
                }
                this.reload(failed, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 底栏「取消」：丢掉草稿和手改的，重读文件。
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.drafts.clear();
        self.nudges = 0;
        self.reload(None, window, cx);
    }

    /// 重读文件。整份写进去没成（`failed`）时编辑框里手改的留着，原因显示在底栏。
    fn reload(&mut self, failed: Option<EngineError>, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        self.settled = store.settled(self.agent);
        self.failure = failed.as_ref().map(describe_write);
        let Some(raw) = &mut self.raw else {
            return;
        };
        let (snapshot, text, error) = read(store, &raw.file);
        raw.snapshot = snapshot;
        raw.loaded = text.clone();
        raw.expected = text.clone();
        raw.error = error;
        if failed.is_none() {
            set_text(raw, text, window, cx);
        }
        self.reparse(cx);
        cx.notify();
    }

    fn kind(&self, store: &Store) -> AgentKind {
        store.agents[self.agent].kind
    }

    /// 单选 agent 现在用的那家。
    fn single(&self, store: &Store) -> Option<usize> {
        store.picked(self.agent)
    }

    /// 手填的输入框此刻对着哪家：单选是当前那家，多选是展开的那块。
    fn target(&self, store: &Store) -> Option<usize> {
        match store.agents[self.agent].mode {
            Mode::Switch => self.single(store),
            Mode::Coexist => self.expanded,
        }
    }

    /// 第 `p` 家：有草稿用草稿，没有用存着的。
    fn provider<'a>(&'a self, store: &'a Store, p: usize) -> &'a Provider {
        let saved = &store.providers[p];
        self.drafts
            .iter()
            .find(|draft| draft.id == saved.id)
            .unwrap_or(saved)
    }

    /// 槽位选了一个模型。目录里没有的先加进目录（decisions「模型」）。
    /// 给一个槽位选模型；`None` 是「留空」，覆盖里存空串，这个环境变量就不写（core 的 `agent::slots`）。
    /// 本来就默认留空的槽位（子代理）留空就是没填，从覆盖里删掉。
    fn set_slot(
        &mut self,
        key: &'static str,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(p) = self.single(store) else {
            return;
        };
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
        if let Some(model) = &model {
            ensure_model(&mut provider, model);
        }
        let auto = claude::SLOTS
            .iter()
            .any(|(slot, _)| slot.key == key && slot.auto);
        let o = provider.overrides.entry(kind).or_default();
        match model {
            None if !auto => {
                o.slots.remove(key);
            }
            model => {
                o.slots.insert(key.to_string(), model.unwrap_or_default());
            }
        }
        if *o == AgentOverride::default() {
            provider.overrides.remove(&kind);
        }
        self.menu = None;
        self.stage(provider, window, cx);
    }

    /// 「恢复默认」：清掉这个 agent 对这家的槽位，回到目录的默认模型。
    fn reset_slots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(p) = self.single(store) else {
            return;
        };
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
        if let Some(o) = provider.overrides.get_mut(&kind) {
            o.slots.clear();
            if *o == AgentOverride::default() {
                provider.overrides.remove(&kind);
            }
        }
        self.stage(provider, window, cx);
    }

    /// 列表型、多选 agent：勾上或取消一个模型。最后一个不能取消。
    fn toggle_model(&mut self, p: usize, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
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
        self.stage(provider, window, cx);
    }

    /// 列表型 agent 的默认模型。不在列表里的先勾上。
    fn set_default(&mut self, p: usize, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
        let mut listed = listed(&provider, kind);
        if !listed.iter().any(|m| m == id) {
            listed.push(id.to_string());
            set_list(&mut provider, kind, listed);
        }
        provider.overrides.entry(kind).or_default().default_model = Some(id.to_string());
        self.stage(provider, window, cx);
    }

    /// 多选 agent：这家走哪种接口。
    fn set_protocol(
        &mut self,
        p: usize,
        protocol: Protocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
        provider.overrides.entry(kind).or_default().protocol = Some(protocol);
        self.stage(provider, window, cx);
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
        self.set_slot(key, Some(value), window, cx);
    }

    /// 列表卡、多选块底下回车：手填的模型加进目录，也勾上。
    fn add_extra(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.extra.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let store = self.store.read(cx);
        let Some(p) = self.target(store) else {
            return;
        };
        let kind = self.kind(store);
        let mut provider = self.provider(store, p).clone();
        ensure_model(&mut provider, &value);
        let mut listed = listed(&provider, kind);
        if !listed.contains(&value) {
            listed.push(value);
            set_list(&mut provider, kind, listed);
        }
        self.extra
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.stage(provider, window, cx);
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
        let _subscription = cx.subscribe(&picker, |this, _, event: &PickerEvent, cx| {
            if let PickerEvent::SaveAs(draft) = event {
                cx.emit(ConfigEvent::SaveAs(draft.clone()));
            }
            this.picker = None;
            cx.notify();
        });
        self.picker = Some(OpenPicker {
            picker,
            _subscription,
        });
        self.menu = None;
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
                raw.error = None;
                set_text(
                    raw,
                    String::from_utf8_lossy(&bytes).into_owned(),
                    window,
                    cx,
                );
            }
            Err(err) => raw.error = Some(describe(&err)),
        }
        self.reparse(cx);
        cx.notify();
    }

    /// 这个槽位显示什么：编辑框里的（没有这个键就是留空）> 读不了时按目录算的。
    fn slot_value(
        &self,
        provider: &Provider,
        kind: AgentKind,
        key: &'static str,
    ) -> Option<String> {
        // 读得了就以编辑框为准：没有这个键就是留空，不拿目录算的顶上。
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

    /// 列表型 agent 的默认模型显示什么：编辑框里的（还在目录里时）> 按覆盖算出来的。
    fn default_value(&self, provider: &Provider, kind: AgentKind) -> Option<String> {
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
                            if this.dirty(cx) {
                                this.nudge(cx);
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
        let kind = self.kind(store);
        match store.agents[self.agent].mode {
            Mode::Switch => {
                let p = store.picked(self.agent)?;
                Some(match kind {
                    AgentKind::ClaudeCode => self.slots_card(p, kind, store, t, cx),
                    _ => self.list_card(p, kind, store, t, window, cx),
                })
            }
            Mode::Coexist => {
                let enabled = store.enabled(self.agent);
                (!enabled.is_empty()).then(|| self.multi_card(&enabled, kind, store, t, window, cx))
            }
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
        let provider = self.provider(store, p);
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
                            .on_click(
                                cx.listener(|this, _, window, cx| this.reset_slots(window, cx)),
                            ),
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
        let mut list =
            div()
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
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_slot(key, None, window, cx)
                        }))
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_slot(key, Some(id.clone()), window, cx)
                    }))
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
        let provider = self.provider(store, p);
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
            let provider = self.provider(store, p);
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
                            cx.listener(move |this, protocol: &Protocol, window, cx| {
                                this.set_protocol(p, *protocol, window, cx)
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
                    .on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.toggle_model(p, &id, window, cx)
                        }),
                    )
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
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_default(p, &id_for_default, window, cx)
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

    /// 「原始文件」卡：标题行（路径、「格式化」），底下是编辑框。
    fn raw_card(&self, t: &Theme, cx: &Context<Self>) -> Option<Div> {
        let raw = self.raw.as_ref()?;
        let path: SharedString = self.store.read(cx).tilde(&raw.file.path).into();
        // 上下各 8 的内边距；行高画过一帧才知道，之前按 12px 字号的 18 算。
        let line = raw.state.read(cx).line_height().map_or(18., f32::from);
        let height = (raw.rows as f32 * line + 16.).max(120.);
        let formattable = raw.file.format != Format::Toml;
        Some(
            ui::list(t, false)
                .pb(px(14.))
                .child(
                    ui::card_title(t, t!("config.raw_file"))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::NORMAL)
                                .text_size(px(11.5))
                                .text_color(t.muted)
                                .child(path),
                        )
                        .child(div().flex_1())
                        .when(formattable, |row| {
                            row.child(
                                ui::text_button("raw-format", t)
                                    .font_weight(FontWeight::NORMAL)
                                    .tooltip(ui::tooltip("⌥⇧F"))
                                    .child(t!("config.format"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.format_raw(window, cx)
                                    })),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .relative()
                        .mx(px(14.))
                        .rounded(px(8.))
                        .bg(t.field)
                        .border_1()
                        .border_color(t.line)
                        .overflow_hidden()
                        .child({
                            let cell = raw.bounds.clone();
                            canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                                .absolute()
                                .size_full()
                        })
                        .child(
                            CodeEditor::new(&raw.state)
                                .appearance(false)
                                .bordered(false)
                                .h(px(height))
                                .text_size(px(12.)),
                        ),
                )
                .when_some(raw.error.clone(), |card, error| {
                    card.child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap(px(5.))
                            .pt(px(10.))
                            .px(px(14.))
                            .text_size(px(11.5))
                            .text_color(t.danger)
                            .child(ui::icon("circle-alert", 13., t.danger))
                            .child(div().min_w_0().truncate().child(error)),
                    )
                }),
        )
    }

    /// 有改动、正在保存或保存没成时，主窗口底下浮着的一条：说明、「取消」「保存」。
    /// 画在主窗口里（这页在滚动区里，跟着滚就看不见了）；提示条往上让出它的高度（`SAVE_BAR_SPACE`）。
    pub fn save_bar(&self, t: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.saving && self.failure.is_none() && !self.dirty(cx) {
            return None;
        }
        let (text, color): (SharedString, Hsla) = match &self.failure {
            Some(failure) => (failure.clone(), t.danger),
            None if self.nudges > 0 => (t!("config.unsaved").into(), t.danger),
            None => (t!("config.unsaved").into(), t.fg_2),
        };
        let nudges = self.nudges;
        let bar = div()
            .id("save-bar")
            .h_flex()
            .items_center()
            .gap(px(8.))
            .h(px(SAVE_BAR_HEIGHT))
            .w(px(520.))
            .pl(px(16.))
            .pr(px(8.))
            .rounded(px(12.))
            .bg(Hsla { a: 1., ..t.pop_bg })
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .occlude()
            .text_size(px(12.5))
            .child(
                div()
                    .id("save-bar-text")
                    .flex_1()
                    .min_w_0()
                    .mr(px(8.))
                    .truncate()
                    .text_color(color)
                    .when(self.failure.is_some(), |d| {
                        d.tooltip(ui::tooltip(text.clone()))
                    })
                    .child(text.clone()),
            )
            .child(
                ui::button("save-bar-cancel", t, Button::Secondary)
                    .flex_none()
                    .w(px(72.))
                    .child(t!("common.cancel"))
                    .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
            )
            .child(
                ui::button("save-bar-save", t, Button::Primary)
                    .flex_none()
                    .w(px(72.))
                    .when(self.saving, |b| b.opacity(0.6))
                    .tooltip(ui::tooltip("⌘S"))
                    .child(t!("common.save"))
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            );
        // 出现时从下面升上来；想离开时左右晃一下。
        let bar = if nudges == 0 {
            bar.with_animation(
                "save-bar-in",
                Animation::new(Duration::from_millis(240)).with_easing(ui::ease_out_quart),
                |el, d| el.opacity(d).mt(px(10. * (1. - d))),
            )
            .into_any_element()
        } else {
            bar.with_animation(
                ElementId::NamedInteger("save-bar-nudge".into(), nudges),
                Animation::new(Duration::from_millis(420)),
                |el, d| el.ml(px((d * PI * 6.).sin() * 8. * (1. - d))),
            )
            .into_any_element()
        };
        Some(
            deferred(
                div()
                    .absolute()
                    .bottom(px(14.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(bar),
            )
            .into_any_element(),
        )
    }
}

impl Render for ConfigPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let models = self.models_card(&t, window, cx);
        let bar = self.saving || self.failure.is_some() || self.dirty(cx);
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
            .children(self.raw_card(&t, cx))
            // 底栏浮着时，滚到底也能看见编辑框的最后几行。
            .when(bar, |page| page.pb(px(SAVE_BAR_SPACE - 16.)))
    }
}

/// 底栏的高度，和它占掉的底部空间（提示条往上让这么多）。
const SAVE_BAR_HEIGHT: f32 = 44.;
pub const SAVE_BAR_SPACE: f32 = SAVE_BAR_HEIGHT + 14. + 10.;

/// 解析不了时只说第几行第几列（编辑框里就是这个文件，不用再写路径）；别的照原话。
fn describe(err: &PatchError) -> SharedString {
    match err {
        PatchError::Parse {
            line,
            column,
            message,
            ..
        } => {
            // serde_json 的原话末尾自带「at line 1 column 1」，前面已经说了。
            let message = message
                .strip_suffix(&format!(" at line {line} column {column}"))
                .unwrap_or(message);
            t!(
                "config.parse_error",
                line = line,
                column = column,
                message = message
            )
            .into()
        }
        other => other.to_string().into(),
    }
}

fn describe_write(err: &EngineError) -> SharedString {
    match err {
        EngineError::Patch(err) => describe(err),
        EngineError::Conflict { .. } => t!("config.conflict").into(),
        other => other.to_string().into(),
    }
}

/// 读这个文件：内容和读出来的文本；读不了时内容是空的，带上原因。
fn read(store: &Store, file: &LiveFile) -> (Snapshot, String, Option<SharedString>) {
    match store.read_file(file) {
        Ok(snapshot) => {
            let text = snapshot
                .bytes
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default();
            (snapshot, text, None)
        }
        Err(err) => (
            Snapshot {
                bytes: None,
                digest: None,
            },
            String::new(),
            Some(err.to_string().into()),
        ),
    }
}

/// 编辑框里有几行：结尾的换行后面还有一个空行，`lines()` 不算它。
fn rows(text: &str) -> usize {
    text.split('\n').count()
}

/// 编辑框换成 `text`；一样就不动（换了会滚回顶上）。
fn set_text(raw: &mut RawEditor, text: String, window: &mut Window, cx: &mut App) {
    if raw.state.read(cx).value().as_ref() == text {
        return;
    }
    raw.rows = rows(&text);
    raw.state
        .update(cx, |state, cx| state.set_value(text, window, cx));
}

/// 这个 agent 在用哪几家（供应商 id）。
fn in_use(store: &Store, agent: usize) -> Vec<String> {
    match &store.agents[agent].status {
        AgentStatus::Provider { id } => vec![id.clone()],
        AgentStatus::Multi { ids } => ids.clone(),
        _ => Vec::new(),
    }
}

/// 槽位的名字（core 的 `claude::SLOTS`）。
fn slot_label(key: &str) -> SharedString {
    match key {
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
