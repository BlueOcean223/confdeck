//! 供应商的弹层编辑器（提案 10 第二节，decisions「供应商页」）：盖在列表上，字段就是 `providers.json` 里的那几项。
//! 覆盖不在这里，在 agent 的「配置 ›」页。
//!
//! - 在用的 agent 一排芯片（名字 + 它用的模型）；名字；ID（新建时从名字生成、可改，建好后只读）；
//!   Key（遮罩 + 显示）；四种接口地址各一行，空着就是不提供；模型目录简表，点一行展开改全部字段，
//!   「添加模型」是同一个表单；每行最前面是默认模型的标记。
//! - 保存走 `Store::save_provider`（core 的 `apply::save_provider`），结果交给主窗口提示。
//! - 删除先列出在用的 agent（`apply::used_by`）确认。有没保存的改动时关掉先问。

use confdeck_core::agent::Mode;
use confdeck_core::apply::{Synced, UsedBy};
use confdeck_core::preset::Preset;
use confdeck_core::provider::{Modality, Model, Protocol, Provider, ProviderError};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Store};
use crate::theme::Theme;
use crate::ui::{self, Button};

pub enum EditorEvent {
    /// 保存好了：ID、名字，和写进了哪些 agent（写失败的也在里面）。
    Saved {
        id: String,
        name: String,
        synced: Vec<Synced>,
    },
    /// 删掉了：名字，和从哪些多选 agent 里移掉了。
    Deleted { name: String, synced: Vec<Synced> },
    /// 取消、Esc、点外面。
    Closed,
}

/// 四种接口，表单里的顺序。
const PROTOCOLS: [Protocol; 4] = [
    Protocol::Anthropic,
    Protocol::OpenaiChat,
    Protocol::OpenaiResponses,
    Protocol::Gemini,
];

pub const WIDTH: f32 = 560.;

/// 模型目录里的一行：收起时是简表，展开是表单。
struct ModelRow {
    id: Entity<InputState>,
    name: Entity<InputState>,
    context: Entity<InputState>,
    output: Entity<InputState>,
    image: bool,
    reasoning: Entity<InputState>,
    /// 原来的输入模态，保存时只增减「图像」。
    input: Vec<Modality>,
}

enum Confirm {
    Discard,
    Delete(UsedBy),
}

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

pub struct Editor {
    store: Entity<Store>,
    /// 打开时的样子，用来判断有没有改动。新建时是预设抄过来的（或空白的）那份。
    initial: Provider,
    is_new: bool,
    preset: Option<&'static Preset>,
    name: Entity<InputState>,
    id: Entity<InputState>,
    /// 用户自己改过 ID，之后不再跟着名字变。
    id_edited: bool,
    key: Entity<InputState>,
    key_visible: bool,
    endpoints: Vec<(Protocol, Entity<InputState>)>,
    models: Vec<ModelRow>,
    /// 默认模型是第几行；`None` 是没指定（目录第一个）。
    default_row: Option<usize>,
    expanded: Option<usize>,
    error: Option<SharedString>,
    confirm: Option<Confirm>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for Editor {}

impl Editor {
    /// 编辑已有的一家。
    pub fn edit(
        store: Entity<Store>,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preset = provider
            .preset
            .as_deref()
            .and_then(confdeck_core::preset::get);
        Self::new(store, provider, false, preset, window, cx)
    }

    /// 新建：有预设就照预设填，没有就是空白的。
    pub fn create(
        store: Entity<Store>,
        preset: Option<&'static Preset>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let provider = match preset {
            Some(preset) => {
                let id = store.read(cx).unique_id(&preset.id);
                preset.provider(&id, "")
            }
            None => Provider {
                id: String::new(),
                name: String::new(),
                preset: None,
                key: String::new(),
                endpoints: Default::default(),
                models: Vec::new(),
                default_model: None,
                overrides: Default::default(),
            },
        };
        Self::new(store, provider, true, preset, window, cx)
    }

    /// 「把当前配置存成供应商」：按 agent 配置文件拼好的一家（core 的 `draft`）当新建的来编辑，
    /// ID、名字、地址、Key、模型都填好了，用户看一眼、补个名字就能存。
    pub fn from_draft(
        store: Entity<Store>,
        provider: Provider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preset = provider
            .preset
            .as_deref()
            .and_then(confdeck_core::preset::get);
        Self::new(store, provider, true, preset, window, cx)
    }

    fn new(
        store: Entity<Store>,
        provider: Provider,
        is_new: bool,
        preset: Option<&'static Preset>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = Theme::window(window.appearance());
        crate::theme::sync_component_theme(&t, cx);
        let mut subscriptions = Vec::new();
        let mut input = |placeholder: SharedString, value: &str, cx: &mut Context<Self>| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.to_string())
            });
            subscriptions.push(cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.error = None;
                    cx.notify();
                }
            }));
            state
        };

        let name = input(t!("editor.name").into(), &provider.name, cx);
        let id = input("id".into(), &provider.id, cx);
        let key = input(t!("editor.key_placeholder").into(), &provider.key, cx);
        let endpoints = PROTOCOLS
            .iter()
            .map(|&protocol| {
                let url = provider.endpoint(protocol).unwrap_or_default();
                (protocol, input(t!("editor.not_offered").into(), url, cx))
            })
            .collect();
        let models = provider
            .models
            .iter()
            .map(|model| Self::model_row(model, &mut input, cx))
            .collect();
        let default_row = provider
            .default_model
            .as_deref()
            .and_then(|id| provider.models.iter().position(|m| m.id == id));

        // 新建时 ID 跟着名字走，直到用户自己动了 ID。
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event
                    && this.is_new
                    && !this.id_edited
                {
                    let slug = slug(&this.name.read(cx).value());
                    this.id.update(cx, |id, cx| id.set_value(slug, window, cx));
                }
            },
        ));
        subscriptions.push(cx.subscribe(&id, |this, _, event: &InputEvent, _| {
            if let InputEvent::Change = event {
                this.id_edited = true;
            }
        }));
        key.update(cx, |key, cx| key.set_masked(true, window, cx));

        // Esc 关掉（有改动先问）。输入框自己也绑了 Esc，在它之前拦下来。
        let editor = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        subscriptions.push(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
                || event.keystroke.key != "escape"
            {
                return;
            }
            if let Some(editor) = editor.upgrade() {
                editor.update(cx, |editor, cx| editor.request_close(cx));
                cx.stop_propagation();
            }
        }));

        // 新建时光标放在 Key（预设）或名字（自定义）上。
        let focus = if is_new && preset.is_some() {
            &key
        } else {
            &name
        };
        focus.update(cx, |input, cx| input.focus(window, cx));

        Self {
            store,
            initial: provider,
            is_new,
            preset,
            name,
            id,
            id_edited: false,
            key,
            key_visible: false,
            endpoints,
            models,
            default_row,
            expanded: None,
            error: None,
            confirm: None,
            _subscriptions: subscriptions,
        }
    }

    fn model_row(
        model: &Model,
        input: &mut impl FnMut(SharedString, &str, &mut Context<Self>) -> Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> ModelRow {
        let number = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
        ModelRow {
            id: input(t!("editor.model_id").into(), &model.id, cx),
            name: input(
                t!("editor.model_name").into(),
                model.name.as_deref().unwrap_or_default(),
                cx,
            ),
            context: input("128000".into(), &number(model.context), cx),
            output: input("32000".into(), &number(model.output), cx),
            image: model.input.contains(&Modality::Image),
            reasoning: input(
                t!("editor.reasoning_placeholder").into(),
                &model.reasoning.join(", "),
                cx,
            ),
            input: model.input.clone(),
        }
    }

    /// 表单现在的内容拼成一家供应商（没整理、没校验）。
    fn draft(&self, cx: &App) -> Provider {
        let value = |state: &Entity<InputState>| state.read(cx).value().trim().to_string();
        let models: Vec<Model> = self
            .models
            .iter()
            .map(|row| {
                let mut input = row.input.clone();
                if row.image {
                    if !input.contains(&Modality::Text) {
                        input.insert(0, Modality::Text);
                    }
                    if !input.contains(&Modality::Image) {
                        input.push(Modality::Image);
                    }
                } else {
                    input.retain(|m| *m != Modality::Image);
                }
                let name = value(&row.name);
                Model {
                    id: value(&row.id),
                    name: (!name.is_empty()).then_some(name),
                    context: parse_tokens(&value(&row.context)),
                    output: parse_tokens(&value(&row.output)),
                    input,
                    reasoning: value(&row.reasoning)
                        .split([',', ' ', '、', '/'])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                }
            })
            .collect();
        let default_model = self
            .default_row
            .and_then(|row| models.get(row))
            .map(|model| model.id.clone());
        Provider {
            id: if self.is_new {
                value(&self.id)
            } else {
                self.initial.id.clone()
            },
            name: value(&self.name),
            preset: self.initial.preset.clone(),
            key: value(&self.key),
            endpoints: self
                .endpoints
                .iter()
                .filter_map(|(protocol, state)| {
                    let url = value(state);
                    (!url.is_empty()).then_some((*protocol, url))
                })
                .collect(),
            models,
            default_model,
            overrides: self.initial.overrides.clone(),
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        let mut draft = self.draft(cx);
        draft.normalize();
        draft != self.initial
    }

    /// 取消、Esc、点外面：有改动先问。
    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            self.confirm = None;
        } else if self.dirty(cx) {
            self.confirm = Some(Confirm::Discard);
        } else {
            cx.emit(EditorEvent::Closed);
            return;
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut provider = self.draft(cx);
        provider.normalize();
        if self.is_new
            && self
                .store
                .read(cx)
                .providers
                .iter()
                .any(|p| p.id == provider.id)
        {
            self.error = Some(describe(&ProviderError::DuplicateId { id: provider.id }));
            cx.notify();
            return;
        }
        if let Err(err) = provider.validate() {
            self.error = Some(describe(&err));
            cx.notify();
            return;
        }
        let id = provider.id.clone();
        let name = provider.name.clone();
        let saved = self.store.update(cx, |store, cx| {
            let result = store.save_provider(&provider);
            cx.notify();
            result
        });
        match saved {
            Ok(synced) => cx.emit(EditorEvent::Saved { id, name, synced }),
            Err(err) => {
                self.error = Some(err.to_string().into());
                cx.notify();
            }
        }
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) {
        let used_by = self.store.read(cx).used_by(&self.initial.id);
        self.confirm = Some(Confirm::Delete(used_by));
        cx.notify();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let id = self.initial.id.clone();
        let name = self.initial.name.clone();
        let deleted = self.store.update(cx, |store, cx| {
            let result = store.delete_provider(&id);
            cx.notify();
            result
        });
        match deleted {
            Ok(synced) => cx.emit(EditorEvent::Deleted { name, synced }),
            Err(err) => {
                self.confirm = None;
                self.error = Some(err.to_string().into());
                cx.notify();
            }
        }
    }

    fn add_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let mut input = |placeholder: SharedString, value: &str, cx: &mut Context<Self>| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.to_string())
            });
            subscriptions.push(cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.error = None;
                    cx.notify();
                }
            }));
            state
        };
        let row = Self::model_row(&Model::default(), &mut input, cx);
        row.id.update(cx, |id, cx| id.focus(window, cx));
        self._subscriptions.extend(subscriptions);
        self.models.push(row);
        self.expanded = Some(self.models.len() - 1);
        cx.notify();
    }

    fn remove_model(&mut self, row: usize, cx: &mut Context<Self>) {
        self.models.remove(row);
        self.default_row = match self.default_row {
            Some(d) if d == row => None,
            Some(d) if d > row => Some(d - 1),
            other => other,
        };
        self.expanded = None;
        cx.notify();
    }

    /// 在用这家的 agent：（下标，模型名）。
    fn users(&self, cx: &App) -> Vec<(usize, Option<String>)> {
        if self.is_new {
            return Vec::new();
        }
        let store = self.store.read(cx);
        let Some(provider) = store.providers.iter().position(|p| p.id == self.initial.id) else {
            return Vec::new();
        };
        store
            .users_of(provider)
            .into_iter()
            .map(|agent| (agent, store.model_for(agent, provider)))
            .collect()
    }

    fn section(&self, t: &Theme, title: SharedString, hint: Option<SharedString>) -> Div {
        div()
            .v_flex()
            .gap(px(2.))
            .mt(px(6.))
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.fg_2)
                    .child(title),
            )
            .children(hint.map(|hint| ui::hint(t, hint)))
    }

    // 标签贴着第一行（输入框）对齐，不是整行居中：右边带说明、说明折行时，居中的标签会悬在半空。
    fn form_row(&self, t: &Theme, label: SharedString, content: impl IntoElement) -> Div {
        div()
            .h_flex()
            .items_start()
            .gap(px(12.))
            .child(ui::label(t, label))
            .child(div().flex_1().min_w_0().child(content))
    }

    fn header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let draft_name = self.name.read(cx).value().trim().to_string();
        // 名字清空时，已有的供应商标题退回原名，新建的才叫「新供应商」。
        let shown_name: SharedString = if !draft_name.is_empty() {
            draft_name.clone().into()
        } else if self.is_new {
            t!("editor.title_new").into()
        } else {
            self.initial.name.clone().into()
        };
        let logo = match self.preset.and_then(|preset| data::preset_logo(&preset.id)) {
            Some(logo) => logo,
            None => {
                let mut draft = self.draft(cx);
                if draft.name.is_empty() {
                    draft.name = "+".into();
                }
                data::logo(&draft)
            }
        };
        let website = self.preset.map(|preset| preset.website.clone());

        // 表单比窗口高时卡片顶到最大高度，顶栏、底栏不许跟着收缩，只让中间滚动。
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(20.))
            .h(px(56.))
            .border_b_1()
            .border_color(t.line_2)
            .child(ui::logo(logo, 24., t, window, cx))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(shown_name),
            )
            .when_some(website, |header, website| {
                let host = confdeck_core::provider::host(&website)
                    .unwrap_or(&website)
                    .to_string();
                header.child(
                    div()
                        .id("website")
                        .h_flex()
                        .items_center()
                        .gap(px(3.))
                        .text_size(px(11.5))
                        .text_color(t.muted)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_click(move |_, _, cx| cx.open_url(&website))
                        .child(host)
                        .child(ui::icon("arrow-up-right", 11., t.faint)),
                )
            })
            .child(div().flex_1())
            .when(!self.is_new, |header| {
                header.child(
                    ui::button("delete", t, Button::Danger)
                        .px(px(8.))
                        .child(t!("editor.delete"))
                        .on_click(cx.listener(|this, _, _, cx| this.ask_delete(cx))),
                )
            })
            .child(
                div()
                    .id("close")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui_kit::transparent_black()))
                    .child(ui::icon("x", 15., t.muted))
                    .on_click(cx.listener(|this, _, _, cx| this.request_close(cx))),
            )
    }

    fn in_use(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Option<Div> {
        let users = self.users(cx);
        if users.is_empty() {
            return None;
        }
        let store = self.store.read(cx);
        Some(
            div()
                .h_flex()
                .items_center()
                .flex_wrap()
                .gap(px(6.))
                .child(ui::label(t, t!("editor.in_use")))
                .children(users.into_iter().map(|(agent, model)| {
                    let agent = &store.agents[agent];
                    ui::chip(t)
                        .child(ui::logo(agent.logo, 14., t, window, cx))
                        .child(div().font_weight(FontWeight::MEDIUM).child(agent.name))
                        .when_some(model, |chip, model| {
                            chip.child(div().text_color(t.muted).child(model))
                        })
                })),
        )
    }

    fn model_line(&self, row: &ModelRow, cx: &App) -> SharedString {
        let value = |state: &Entity<InputState>| state.read(cx).value().trim().to_string();
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = parse_tokens(&value(&row.context)) {
            parts.push(tokens(n));
        }
        if let Some(n) = parse_tokens(&value(&row.output)) {
            parts.push(tokens(n));
        }
        if row.image {
            parts.push(t!("editor.image").into());
        }
        let reasoning = value(&row.reasoning);
        if !reasoning.is_empty() {
            let levels = reasoning
                .split([',', ' ', '、', '/'])
                .filter(|s| !s.trim().is_empty())
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" / ");
            parts.push(t!("editor.reasoning_line", levels = levels).into());
        }
        if parts.is_empty() {
            t!("editor.model_no_details").into()
        } else {
            parts.join(" · ").into()
        }
    }

    fn model_rows(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let effective_default = self.default_row.or((!self.models.is_empty()).then_some(0));
        // overflow_hidden 让 Taffy 把最小高度算成 0，在滚动的表单里会被压扁，所以不许收缩。
        let mut list = div()
            .v_flex()
            .flex_none()
            .rounded(px(9.))
            .border_1()
            .border_color(t.line)
            .overflow_hidden();
        if self.models.is_empty() {
            list = list.child(
                div()
                    .py(px(14.))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .text_center()
                    .child(t!("editor.models_empty")),
            );
        }
        for (n, row) in self.models.iter().enumerate() {
            let expanded = self.expanded == Some(n);
            let is_default = effective_default == Some(n);
            let id_text = row.id.read(cx).value().trim().to_string();
            let name_text = row.name.read(cx).value().trim().to_string();
            let hover_bg = t.card_2;
            let radio = div()
                .id(ElementId::named_usize("default", n))
                .flex_none()
                .size(px(14.))
                .rounded_full()
                .border_1()
                .border_color(if is_default { t.accent } else { t.faint })
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .tooltip(ui::tooltip(t!("editor.default_model")))
                .when(is_default, |r| {
                    r.child(div().size(px(7.)).rounded_full().bg(t.accent))
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.default_row = Some(n);
                    cx.notify();
                }));
            let summary = div()
                .id(ElementId::named_usize("model", n))
                .h_flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .h(px(40.))
                .px(px(12.))
                .when(n > 0, |r| r.border_t_1().border_color(t.line_2))
                .cursor_pointer()
                .when(!expanded, |r| r.hover(move |s| s.bg(hover_bg)))
                .when(expanded, |r| r.bg(t.card_2))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.expanded = if this.expanded == Some(n) {
                        None
                    } else {
                        Some(n)
                    };
                    cx.notify();
                }))
                .child(radio)
                .child(
                    div()
                        .h_flex()
                        .items_baseline()
                        .gap(px(8.))
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .font_family("Menlo")
                                .text_size(px(12.))
                                .child(if id_text.is_empty() {
                                    t!("editor.model_unnamed").to_string()
                                } else {
                                    id_text
                                }),
                        )
                        .when(!name_text.is_empty(), |d| {
                            d.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(t.muted)
                                    .child(name_text),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(t.muted)
                        .child(self.model_line(row, cx)),
                )
                .child(ui::icon(
                    if expanded {
                        "chevron-up"
                    } else {
                        "chevron-down"
                    },
                    12.,
                    t.faint,
                ));
            list = list.child(summary);
            if expanded {
                list = list.child(self.model_form(n, row, t, window, cx));
            }
        }
        list
    }

    fn model_form(
        &self,
        n: usize,
        row: &ModelRow,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let field =
            |state: &Entity<InputState>, mono: bool| ui::field(t, state, mono, None, window, cx);
        let image = row.image;
        div()
            .v_flex()
            .gap(px(8.))
            .px(px(12.))
            .pb(px(12.))
            .pt(px(4.))
            .bg(t.card_2)
            .border_t_1()
            .border_color(t.line_2)
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(div().flex_1().child(field(&row.id, true)))
                    .child(div().flex_1().child(field(&row.name, false))),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(ui::hint(t, t!("editor.context")).flex_none())
                            .child(div().flex_1().child(field(&row.context, true))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_flex()
                            .items_center()
                            .gap(px(8.))
                            .child(ui::hint(t, t!("editor.output")).flex_none())
                            .child(div().flex_1().child(field(&row.output, true))),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id(ElementId::named_usize("image", n))
                            .h_flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .child(ui::checkbox(t, image))
                            .child(div().text_size(px(12.)).child(t!("editor.image_input")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.models[n].image = !image;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(ui::hint(t, t!("editor.reasoning")).flex_none())
                    .child(div().w(px(220.)).child(field(&row.reasoning, true))),
            )
            .child(
                div().h_flex().justify_end().child(
                    ui::button(ElementId::named_usize("remove-model", n), t, Button::Danger)
                        .px(px(8.))
                        .h(px(24.))
                        .text_size(px(12.))
                        .child(t!("editor.remove_model"))
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_model(n, cx))),
                ),
            )
    }

    fn body(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let store = self.store.read(cx);
        let users = self.users(cx);
        let user_names: Vec<&str> = users
            .iter()
            .map(|(agent, _)| store.agents[*agent].name)
            .collect();
        let field =
            |state: &Entity<InputState>, mono: bool| ui::field(t, state, mono, None, window, cx);

        let id_row = if self.is_new {
            self.form_row(
                t,
                t!("editor.id").into(),
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(field(&self.id, true))
                    .child(ui::hint(t, t!("editor.id_hint_new"))),
            )
        } else {
            self.form_row(
                t,
                t!("editor.id").into(),
                div().v_flex().gap(px(2.)).child(
                    div()
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .font_family("Menlo")
                        .text_size(px(12.5))
                        .child(self.initial.id.clone()),
                ),
            )
        };

        let eye = div()
            .id("key-eye")
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(3.))
            .text_size(px(11.5))
            .text_color(t.muted)
            .cursor_pointer()
            .hover(|s| s.text_color(gpui_kit::transparent_black()))
            .child(ui::icon(
                if self.key_visible { "eye-off" } else { "eye" },
                13.,
                t.muted,
            ))
            .child(if self.key_visible {
                t!("editor.key_hide")
            } else {
                t!("editor.key_show")
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.key_visible = !this.key_visible;
                let visible = this.key_visible;
                this.key
                    .update(cx, |key, cx| key.set_masked(!visible, window, cx));
                cx.notify();
            }));
        let key_row = self.form_row(
            t,
            t!("editor.key").into(),
            div()
                .v_flex()
                .gap(px(4.))
                .child(ui::field(
                    t,
                    &self.key,
                    true,
                    Some(eye.into_any_element()),
                    window,
                    cx,
                ))
                .when(!user_names.is_empty(), |d| {
                    let agents = user_names.join(&t!("common.list_separator"));
                    d.child(ui::hint(t, t!("editor.key_hint_in_use", agents = agents)))
                }),
        );

        let mut body = div()
            .id("editor-body")
            .v_flex()
            .gap(px(12.))
            .px(px(20.))
            .py(px(16.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        body = body
            .children(self.in_use(t, window, cx))
            .child(self.form_row(t, t!("editor.name").into(), field(&self.name, false)))
            .child(id_row)
            .child(key_row)
            .child(self.section(
                t,
                t!("editor.endpoints").into(),
                Some(t!("editor.endpoints_hint").into()),
            ));
        for (protocol, state) in &self.endpoints {
            body = body.child(self.form_row(t, protocol.to_string().into(), field(state, true)));
        }
        body = body
            .child(self.section(t, t!("editor.models").into(), None))
            .child(self.model_rows(t, window, cx))
            .child(
                ui::text_button("add-model", t)
                    .child(ui::icon("plus", 13., t.ctl_fg))
                    .child(t!("editor.add_model"))
                    .on_click(cx.listener(|this, _, window, cx| this.add_model(window, cx))),
            );
        body
    }

    fn footer(&self, t: &Theme, cx: &Context<Self>) -> Div {
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(20.))
            .h(px(56.))
            .border_t_1()
            .border_color(t.line_2)
            .child(match &self.error {
                Some(error) => div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(11.5))
                    .text_color(t.danger)
                    .child(ui::icon("circle-alert", 13., t.danger))
                    .child(div().min_w_0().truncate().child(error.clone())),
                None => div()
                    .flex_1()
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(t!("editor.footer_hint")),
            })
            .child(
                ui::button("cancel", t, Button::Secondary)
                    .child(t!("common.cancel"))
                    .on_click(cx.listener(|this, _, _, cx| this.request_close(cx))),
            )
            .child(
                ui::button("save", t, Button::Primary)
                    .child(t!("common.save"))
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }

    /// 盖在编辑器上的确认框：放弃改动、删除。
    fn confirm_card(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Option<Div> {
        let confirm = self.confirm.as_ref()?;
        let store = self.store.read(cx);
        let card = div()
            .v_flex()
            .gap(px(10.))
            .w(px(380.))
            .p(px(18.))
            .rounded(px(12.))
            .bg(Hsla { a: 1., ..t.pop_bg })
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let title = |text: SharedString| {
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(text)
        };
        let buttons = |cancel: SharedString, ok: SharedString, on_ok: OnClick| {
            div()
                .h_flex()
                .justify_end()
                .gap(px(8.))
                .mt(px(4.))
                .child(
                    ui::button("confirm-cancel", t, Button::Secondary)
                        .child(cancel)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm = None;
                            cx.notify();
                        })),
                )
                .child(
                    ui::button("confirm-ok", t, Button::Danger)
                        .bg(t.danger.opacity(0.12))
                        .child(ok)
                        .on_click(on_ok),
                )
        };
        let card = match confirm {
            Confirm::Discard => card
                .child(title(t!("editor.discard_title").into()))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(t.fg_2)
                        .child(t!("editor.discard_body")),
                )
                .child(buttons(
                    t!("editor.keep_editing").into(),
                    t!("editor.discard").into(),
                    Box::new(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::Closed))),
                )),
            Confirm::Delete(used_by) => {
                let agent_of = |kind| store.agent_index(kind).map(|i| &store.agents[i]);
                let (switch, coexist): (Vec<_>, Vec<_>) = used_by
                    .agents
                    .iter()
                    .filter_map(|&kind| agent_of(kind))
                    .partition(|agent| agent.mode == Mode::Switch);
                let names = |agents: &[&data::Agent]| {
                    agents
                        .iter()
                        .map(|a| a.name)
                        .collect::<Vec<_>>()
                        .join(&t!("common.list_separator"))
                };
                let host = data::host(&self.initial).unwrap_or_default().to_string();
                let mut card = card.child(title(
                    t!("editor.delete_title", name = self.initial.name.clone()).into(),
                ));
                if used_by.agents.is_empty() && used_by.unknown.is_empty() {
                    card = card.child(
                        div()
                            .text_size(px(12.5))
                            .text_color(t.fg_2)
                            .child(t!("editor.delete_unused")),
                    );
                } else {
                    card = card
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(t.fg_2)
                                .child(t!("editor.delete_used_by")),
                        )
                        .child(div().h_flex().flex_wrap().gap(px(6.)).children(
                            switch.iter().chain(&coexist).map(|agent| {
                                ui::chip(t)
                                    .child(ui::logo(agent.logo, 14., t, window, cx))
                                    .child(div().font_weight(FontWeight::MEDIUM).child(agent.name))
                            }),
                        ));
                    if !switch.is_empty() {
                        card = card.child(ui::hint(
                            t,
                            t!(
                                "editor.delete_switch_note",
                                agents = names(&switch),
                                host = host
                            ),
                        ));
                    }
                    if !coexist.is_empty() {
                        card = card.child(ui::hint(
                            t,
                            t!("editor.delete_coexist_note", agents = names(&coexist)),
                        ));
                    }
                    for (kind, err) in &used_by.unknown {
                        card = card.child(div().text_size(px(11.)).text_color(t.danger).child(t!(
                            "editor.delete_unknown",
                            agent = kind.to_string(),
                            error = err.to_string()
                        )));
                    }
                }
                card.child(buttons(
                    t!("common.cancel").into(),
                    t!("editor.delete").into(),
                    Box::new(cx.listener(|this, _, _, cx| this.delete(cx))),
                ))
            }
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(14.))
                .bg(t.backdrop)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.confirm = None;
                        cx.notify();
                    }),
                )
                .child(card),
        )
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let max_h = f32::from(window.viewport_size().height) - 72.;
        div()
            .relative()
            .v_flex()
            .w(px(WIDTH))
            .max_h(px(max_h))
            .bg(t.card)
            .rounded(px(14.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .text_color(t.fg)
            .text_size(px(13.))
            .child(self.header(&t, window, cx))
            .child(self.body(&t, window, cx))
            .child(self.footer(&t, cx))
            .children(self.confirm_card(&t, window, cx))
    }
}

/// 从名字生成 ID：小写字母和数字留下，其余变成连字符。全是中文时得不到字母，用 `provider`。
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "provider".into()
    } else {
        out
    }
}

/// 解析 token 数：`128000`、`128,000`、`128k`、`1m`。
fn parse_tokens(text: &str) -> Option<u64> {
    let text = text.trim().replace([',', '_', ' '], "").to_lowercase();
    if text.is_empty() {
        return None;
    }
    let (digits, factor) = match text.strip_suffix('m') {
        Some(rest) => (rest.to_string(), 1_000_000),
        None => match text.strip_suffix('k') {
            Some(rest) => (rest.to_string(), 1_000),
            None => (text, 1),
        },
    };
    let value: f64 = digits.parse().ok()?;
    Some((value * factor as f64).round() as u64)
}

/// 简表里显示的 token 数：`128K`、`384K`、`1M`。十进制的整数倍先按十进制算（128000 → 128K），
/// 不然按二进制（131072 → 128K、1048576 → 1M）；两样都不是就原样显示。
pub fn tokens(n: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * KI;
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        return format!("{}M", n / 1_000_000);
    }
    if n >= MI && n.is_multiple_of(MI) {
        return format!("{}M", n / MI);
    }
    if n >= 1_000 && n.is_multiple_of(1_000) {
        return format!("{}K", n / 1_000);
    }
    if n >= KI && n.is_multiple_of(KI) {
        return format!("{}K", n / KI);
    }
    n.to_string()
}

/// 校验错误的文案。
fn describe(err: &ProviderError) -> SharedString {
    match err {
        ProviderError::BadId { .. } => t!("errors.bad_id"),
        ProviderError::DuplicateId { id } => t!("errors.duplicate_id", id = id),
        ProviderError::EmptyName { .. } => t!("errors.empty_name"),
        ProviderError::NoEndpoint { .. } => t!("errors.no_endpoint"),
        ProviderError::BadUrl { protocol, url, .. } => {
            t!("errors.bad_url", protocol = protocol.to_string(), url = url)
        }
        ProviderError::EmptyModelId { .. } => t!("errors.empty_model_id"),
        ProviderError::DuplicateModel { model, .. } => t!("errors.duplicate_model", model = model),
        ProviderError::UnknownModel { agent, model, .. } => {
            t!(
                "errors.unknown_model",
                agent = agent.to_string(),
                model = model
            )
        }
        ProviderError::UnknownDefaultModel { model, .. } => {
            t!("errors.unknown_default_model", model = model)
        }
    }
    .into()
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui 也导出一个 `test` 属性宏，会把 `#[test]` 抢走。
    use super::{parse_tokens, slug, tokens};

    #[test]
    fn slugs_come_from_the_name() {
        assert_eq!(slug("My Relay"), "my-relay");
        assert_eq!(slug("  Kimi For Coding "), "kimi-for-coding");
        assert_eq!(slug("智谱 GLM"), "glm");
        assert_eq!(slug("火山方舟"), "provider");
        assert_eq!(slug("a--b"), "a-b");
    }

    #[test]
    fn token_counts_round_trip() {
        assert_eq!(parse_tokens("128000"), Some(128_000));
        assert_eq!(parse_tokens("128,000"), Some(128_000));
        assert_eq!(parse_tokens("128k"), Some(128_000));
        assert_eq!(parse_tokens("1M"), Some(1_000_000));
        assert_eq!(parse_tokens("1.5m"), Some(1_500_000));
        assert_eq!(parse_tokens(""), None);
        assert_eq!(parse_tokens("abc"), None);
        assert_eq!(tokens(1_048_576), "1M");
        assert_eq!(tokens(131_072), "128K");
        assert_eq!(tokens(200_000), "200K");
        assert_eq!(tokens(384_000), "384K");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(65_536), "64K");
        assert_eq!(tokens(128_000), "128K");
        assert_eq!(tokens(196_608), "192K");
        assert_eq!(tokens(12_345), "12345");
    }
}
