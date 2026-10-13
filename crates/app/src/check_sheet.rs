//! 「额度查询」：一家供应商、一个官方账号的额度或余额查不查、怎么查、多久自动查一次（10-13 用户要的：按供应商配置，
//! 参考 cc-switch 的「配置用量查询」，界面自己画）。供应商行、官方账号行的「⋯」里打开，和编辑器一样盖在页面上。
//!
//! - 「查询」开关：关了这一行那一格空着，点刷新也不查。
//! - 「方式」（只有供应商有，10-13 用户：照 cc-switch 做模板）：认得的几家有「内置」；别的选通用、NewAPI、自定义，
//!   各填各的（[`confdeck_core::query`]）。自定义没填够时「保存」点不了。
//! - 「自动查询」：不自动、1 / 5 / 15 / 30 分钟、1 小时。没设过的显示默认（在用的 5 分钟，不在用的不自动），
//!   没动它就还是默认，保存时不写。
//! - 「现在」一行是这家现在的读数，右边一个圆形箭头马上查一次（不用先保存），查着的时候转。查法改了没保存时，箭头按卡片上填的
//!   试查一次，结果只写在这一行（像 cc-switch 的「测试脚本」），保存以后才算数。
//!   查询关着时这一行变淡、箭头藏起来，卡片高矮不变（不然「保存」会挪位置）。

use confdeck_core::balance::{Balance, BalanceError, Currency};
use confdeck_core::query::{self, Query, Template};
use confdeck_core::quota::{Check, CheckKey};
use confdeck_core::readings;
use gpui_kit::component::input::{InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Logo, Store};
use crate::quota_view::{self, Shown};
use crate::theme::Theme;
use crate::ui::{self, Button};

/// 卡片的宽度：「自动查询」一行放得下六档。
const WIDTH: f32 = 560.;
/// 左边名字那一列。
const LABEL: f32 = 88.;

/// 「自动查询」的几档（分钟），0 是不自动。
const EVERY: [u32; 6] = [0, 1, 5, 15, 30, 60];

pub enum CheckEvent {
    Closed,
}

/// 供应商的查法填在哪。
struct Form {
    /// 这家有认得的查法：「方式」里有「内置」。
    built_in: bool,
    /// 选的方式，`None` 是内置。
    template: Option<Template>,
    post: bool,
    unit: Currency,
    url: Entity<InputState>,
    token: Entity<InputState>,
    user: Entity<InputState>,
    left: Entity<InputState>,
    used: Entity<InputState>,
    divisor: Entity<InputState>,
    headers: Entity<TextareaState>,
    body: Entity<TextareaState>,
    /// 供应商地址的根，占位文字用。
    root: String,
}

impl Form {
    /// 地址、令牌的占位文字：各模板空着时用的是什么。
    fn placeholders(&self, window: &mut Window, cx: &mut App) {
        let root = if self.root.is_empty() {
            "https://…"
        } else {
            self.root.as_str()
        };
        let (url, token): (String, SharedString) = match self.template {
            Some(Template::General) => {
                (format!("{root}/user/balance"), t!("check.key_hint").into())
            }
            Some(Template::Custom) => ("{{baseUrl}}/api/balance".into(), "".into()),
            _ => (root.to_string(), t!("check.optional").into()),
        };
        self.url
            .update(cx, |state, cx| state.set_placeholder(url, window, cx));
        self.token
            .update(cx, |state, cx| state.set_placeholder(token, window, cx));
    }
}

/// 按没保存的查法试查的结果。
enum Trial {
    Running,
    Done(Result<Balance, BalanceError>),
}

pub struct CheckSheet {
    store: Entity<Store>,
    target: CheckKey,
    name: SharedString,
    logo: Logo,
    on: bool,
    every: u32,
    /// 动过「自动查询」：保存时写下来；没动过就留着原来的（没设过的还是默认）。
    touched: bool,
    form: Option<Form>,
    trial: Option<Trial>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CheckEvent> for CheckSheet {}

impl CheckSheet {
    pub fn new(
        store: Entity<Store>,
        target: CheckKey,
        name: SharedString,
        logo: Logo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (check, in_use) = {
            let store = store.read(cx);
            (store.check_of(&target), store.in_use(&target))
        };
        // Esc 关掉。
        let sheet = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
                || event.keystroke.key != "escape"
            {
                return;
            }
            if let Some(sheet) = sheet.upgrade() {
                sheet.update(cx, |_, cx| cx.emit(CheckEvent::Closed));
                cx.stop_propagation();
            }
        });
        let readings = store.read(cx).readings.clone();
        let mut subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&readings, |_, _, cx| cx.notify()),
            intercept,
        ];
        let form = match &target {
            CheckKey::Provider(id) => {
                let provider = store
                    .read(cx)
                    .providers
                    .iter()
                    .find(|p| &p.id == id)
                    .cloned();
                provider.map(|provider| {
                    let built_in = readings::built_in(&provider);
                    let root = query::root(&provider).unwrap_or_default();
                    Self::form(
                        check.query.clone(),
                        built_in,
                        root,
                        &mut subscriptions,
                        window,
                        cx,
                    )
                })
            }
            _ => None,
        };
        Self {
            _subscriptions: subscriptions,
            store,
            target,
            name,
            logo,
            on: !check.off,
            every: check
                .every
                .unwrap_or(if in_use { Check::DEFAULT_EVERY } else { 0 }),
            touched: false,
            form,
            trial: None,
        }
    }

    fn form(
        saved: Option<Query>,
        built_in: bool,
        root: String,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Form {
        let template = match &saved {
            Some(query) => Some(query.template),
            None if built_in => None,
            // 认不出的站点多半是 new-api 一类的中转。
            None => Some(Template::NewApi),
        };
        let saved = saved.unwrap_or_default();
        // 改了查法，上次试查的结果不算了。
        let mut input = |value: &str, hint: SharedString, cx: &mut Context<Self>| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(hint)
                    .default_value(value.to_string())
            });
            subscriptions.push(cx.subscribe(
                &state,
                |this: &mut Self, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        this.trial = None;
                        cx.notify();
                    }
                },
            ));
            state
        };
        let optional: SharedString = t!("check.optional").into();
        // 地址、令牌的占位文字跟着模板换（[`Form::placeholders`]）。
        let url = input(&saved.url, "".into(), cx);
        let token = input(&saved.token, "".into(), cx);
        let user = input(&saved.user, optional.clone(), cx);
        let left = input(&saved.left, "data.balance".into(), cx);
        let used = input(&saved.used, optional, cx);
        let divisor = input(
            &saved.divisor.map(|d| d.to_string()).unwrap_or_default(),
            "1".into(),
            cx,
        );
        let mut area = |value: &str, hint: &str, cx: &mut Context<Self>| {
            let state = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(hint.to_string())
                    .default_value(value.to_string())
            });
            let sub = cx.subscribe(&state, |this: &mut Self, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.trial = None;
                    cx.notify();
                }
            });
            subscriptions.push(sub);
            state
        };
        let headers = area(&saved.headers, "Authorization: Bearer {{apiKey}}", cx);
        let body = area(&saved.body, "{}", cx);
        let form = Form {
            built_in,
            template,
            post: saved.post,
            unit: saved.unit,
            url,
            token,
            user,
            left,
            used,
            divisor,
            headers,
            body,
            root,
        };
        form.placeholders(window, cx);
        form
    }

    /// 卡片上填的查法；内置的是 `None`。只留这个模板用得上的几项。
    fn query(&self, cx: &App) -> Option<Query> {
        let form = self.form.as_ref()?;
        let template = form.template?;
        let value = |state: &Entity<InputState>| state.read(cx).value().trim().to_string();
        let mut query = Query {
            template,
            url: value(&form.url),
            unit: form.unit,
            ..Default::default()
        };
        match template {
            Template::General => query.token = value(&form.token),
            Template::NewApi => {
                query.token = value(&form.token);
                query.user = value(&form.user);
            }
            Template::Custom => {
                query.post = form.post;
                query.headers = form.headers.read(cx).value().trim().to_string();
                if form.post {
                    query.body = form.body.read(cx).value().trim().to_string();
                }
                query.left = value(&form.left);
                query.used = value(&form.used);
                query.divisor = value(&form.divisor).parse().ok().filter(|d: &f64| *d != 0.);
            }
        }
        Some(query)
    }

    /// 查法改了还没保存。
    fn changed(&self, cx: &App) -> bool {
        self.form.is_some() && self.query(cx) != self.store.read(cx).check_of(&self.target).query
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        if query.as_ref().is_some_and(|q| !q.complete()) {
            return;
        }
        let old = self.store.read(cx).check_of(&self.target);
        let check = Check {
            off: !self.on,
            every: if self.touched {
                Some(self.every)
            } else {
                old.every
            },
            query: if self.form.is_some() {
                query
            } else {
                old.query
            },
        };
        let target = self.target.clone();
        self.store
            .update(cx, |store, cx| store.set_check(&target, check, cx));
        cx.emit(CheckEvent::Closed);
    }

    /// 圆形箭头：查法没改是查这一个（结果进供应商页）；改了是按卡片上填的试查。
    fn run(&mut self, cx: &mut Context<Self>) {
        if !self.on {
            return;
        }
        if !self.changed(cx) {
            let target = self.target.clone();
            let readings = self.store.read(cx).readings.clone();
            readings.update(cx, |readings, cx| readings.check_one(&target, cx));
            return;
        }
        if matches!(self.trial, Some(Trial::Running)) {
            return;
        }
        let CheckKey::Provider(id) = &self.target else {
            return;
        };
        let (Some(query), Some(provider)) = (
            self.query(cx),
            self.store
                .read(cx)
                .providers
                .iter()
                .find(|p| &p.id == id)
                .cloned(),
        ) else {
            return;
        };
        if !query.complete() {
            return;
        }
        let proxy = self.store.read(cx).proxy();
        let http = self.store.read(cx).net.http.clone();
        self.trial = Some(Trial::Running);
        let task = cx.background_executor().spawn(async move {
            http.get(&proxy)
                .map_err(BalanceError::Net)
                .and_then(|http| query::run(&http, &provider, &query))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // 等的时候又改了：这个结果不算。
                if matches!(this.trial, Some(Trial::Running)) {
                    this.trial = Some(Trial::Done(result));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(20.))
            .h(px(60.))
            .border_b_1()
            .border_color(t.line_2)
            .child(ui::logo(self.logo, 24., t, window, cx))
            .child(
                div()
                    .v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("check.title")),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .truncate()
                            .child(self.name.clone()),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("close-check")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .child(ui::icon("x", 15., t.muted))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CheckEvent::Closed))),
            )
    }

    /// 一行：左边名字，右边控件。
    fn line(t: &Theme, label: SharedString, control: impl IntoElement) -> Div {
        // 不压扁：内容比卡片高时中间那块滚动。
        div()
            .flex_none()
            .h_flex()
            .items_center()
            .gap(px(12.))
            .min_h(px(32.))
            .child(
                div()
                    .w(px(LABEL))
                    .flex_none()
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .text_color(t.fg)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .justify_end()
                    .child(control),
            )
    }

    /// 输入框：占满右边。
    fn input(t: &Theme, state: &Entity<InputState>, window: &Window, cx: &App) -> Div {
        ui::field(t, state, true, None, window, cx).w_full()
    }

    /// 多行输入框（请求头、请求体）。
    fn area(t: &Theme, state: &Entity<TextareaState>, window: &Window, cx: &App) -> Div {
        let focused = state.read(cx).focus_handle(cx).is_focused(window);
        // 自己按行数定高（2 到 5 行）：让它自己长高时，这一行量出来的高度不对，会压到上下两行。
        let lines = state.read(cx).value().lines().count().clamp(2, 5) as f32;
        div()
            .w_full()
            .h(px(lines * 17. + 14.))
            .px(px(9.))
            .py(px(6.))
            .rounded(px(7.))
            .bg(t.field)
            .border_1()
            .border_color(if focused { t.accent } else { t.line })
            .text_size(px(12.5))
            .font_family("Menlo")
            .child(
                Textarea::new(state)
                    .appearance(false)
                    .xsmall()
                    .h_full()
                    .text_size(px(12.5)),
            )
    }

    /// 选「方式」、方法、单位的分段：改了以后上次试查的结果不算。`id` 隔开几个分段里同名的格子（都叫 `seg-<下标>`）。
    fn pick<T: Copy + PartialEq + 'static>(
        id: &'static str,
        t: &Theme,
        items: &[(T, SharedString)],
        active: T,
        window: &mut Window,
        cx: &mut Context<Self>,
        set: fn(&mut Form, T, &mut Window, &mut App),
    ) -> Stateful<Div> {
        let entity = cx.entity().downgrade();
        div()
            .id(id)
            .flex_none()
            .rounded(px(8.))
            .bg(t.pill)
            .child(ui::seg(
                t,
                items,
                active,
                None,
                window,
                move |value, window, cx| {
                    let value = *value;
                    if let Some(sheet) = entity.upgrade() {
                        sheet.update(cx, |sheet, cx| {
                            if let Some(form) = &mut sheet.form {
                                set(form, value, window, cx);
                            }
                            sheet.trial = None;
                            cx.notify();
                        });
                    }
                },
            ))
    }

    /// 「方式」和各模板要填的几行。
    fn form_rows(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Vec<Div> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut kinds: Vec<(Option<Template>, SharedString)> = Vec::new();
        if form.built_in {
            kinds.push((None, t!("check.built_in").into()));
        }
        kinds.extend([
            (Some(Template::General), t!("check.general").into()),
            (Some(Template::NewApi), "NewAPI".into()),
            (Some(Template::Custom), t!("check.custom").into()),
        ]);
        rows.push(Self::line(
            t,
            t!("check.template").into(),
            Self::pick(
                "template",
                t,
                &kinds,
                form.template,
                window,
                cx,
                |form, template, window, cx| {
                    form.template = template;
                    form.placeholders(window, cx);
                },
            ),
        ));
        let Some(template) = form.template else {
            return rows;
        };
        match template {
            Template::General => {
                rows.push(Self::line(
                    t,
                    t!("check.url").into(),
                    Self::input(t, &form.url, window, cx),
                ));
                rows.push(Self::line(
                    t,
                    "Key".into(),
                    Self::input(t, &form.token, window, cx),
                ));
            }
            Template::NewApi => {
                rows.push(Self::line(
                    t,
                    t!("check.url").into(),
                    Self::input(t, &form.url, window, cx),
                ));
                rows.push(Self::line(
                    t,
                    t!("check.token").into(),
                    Self::input(t, &form.token, window, cx),
                ));
                rows.push(Self::line(
                    t,
                    t!("check.user").into(),
                    Self::input(t, &form.user, window, cx),
                ));
            }
            Template::Custom => {
                let methods: [(bool, SharedString); 2] =
                    [(false, "GET".into()), (true, "POST".into())];
                rows.push(Self::line(
                    t,
                    t!("check.request").into(),
                    div()
                        .w_full()
                        .h_flex()
                        .items_center()
                        .gap(px(8.))
                        .child(Self::pick(
                            "method",
                            t,
                            &methods,
                            form.post,
                            window,
                            cx,
                            |form, post, _, _| form.post = post,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Self::input(t, &form.url, window, cx)),
                        ),
                ));
                rows.push(Self::line(
                    t,
                    t!("check.headers").into(),
                    Self::area(t, &form.headers, window, cx),
                ));
                if form.post {
                    rows.push(Self::line(
                        t,
                        t!("check.body").into(),
                        Self::area(t, &form.body, window, cx),
                    ));
                }
                rows.push(Self::line(
                    t,
                    t!("check.left").into(),
                    Self::input(t, &form.left, window, cx),
                ));
                rows.push(Self::line(
                    t,
                    t!("check.used").into(),
                    Self::input(t, &form.used, window, cx),
                ));
                rows.push(Self::line(
                    t,
                    t!("check.divisor").into(),
                    Self::input(t, &form.divisor, window, cx),
                ));
            }
        }
        let units: [(Currency, SharedString); 3] = [
            (Currency::Usd, t!("check.usd").into()),
            (Currency::Cny, t!("check.cny").into()),
            (Currency::Plain, t!("check.plain").into()),
        ];
        rows.push(Self::line(
            t,
            t!("check.unit").into(),
            Self::pick(
                "unit",
                t,
                &units,
                form.unit,
                window,
                cx,
                |form, unit, _, _| form.unit = unit,
            ),
        ));
        rows
    }
}

impl Render for CheckSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let max_h = f32::from(window.viewport_size().height) - 72.;
        let on = self.on;
        let items: Vec<(u32, SharedString)> = EVERY
            .iter()
            .map(|&every| {
                let label: SharedString = match every {
                    0 => t!("check.never").into(),
                    60 => t!("check.hour").into(),
                    n => t!("check.minutes", n = n).into(),
                };
                (every, label)
            })
            .collect();
        let entity = cx.entity().downgrade();
        let seg = ui::seg(&t, &items, self.every, None, window, move |every, _, cx| {
            let every = *every;
            if let Some(sheet) = entity.upgrade() {
                sheet.update(cx, |sheet, cx| {
                    sheet.every = every;
                    sheet.touched = true;
                    cx.notify();
                });
            }
        });

        // 读数自己画，箭头另画：保存之前开关、查法是卡片上的，不是存着的。
        let (meters, checking) = if self.changed(cx) {
            match &self.trial {
                Some(Trial::Done(Ok(found))) => (
                    quota_view::meters(&t, "check-now", &Shown::Balance(found)),
                    false,
                ),
                Some(Trial::Done(Err(err))) => (
                    quota_view::meters(&t, "check-now", &Shown::BalanceFailed(err)),
                    false,
                ),
                Some(Trial::Running) => (div(), true),
                None => (div(), false),
            }
        } else {
            let readings = self.store.read(cx).readings.read(cx);
            let meters = match readings.of(&self.target) {
                Some(reading) if !matches!(reading.shown, Shown::Off) => {
                    quota_view::meters(&t, "check-now", &reading.shown)
                }
                _ => div(),
            };
            let checking = readings
                .of(&self.target)
                .is_some_and(|reading| reading.checking);
            (meters, checking)
        };
        let complete = self.query(cx).is_none_or(|q| q.complete());
        let now = div()
            .h_flex()
            .items_center()
            .gap(px(8.))
            .child(meters)
            .child(
                ui::icon_button_with(
                    "check-run",
                    &t,
                    ui::spinning(
                        ui::icon("rotate-cw", 15., t.ctl_fg),
                        "check-run-spin",
                        checking,
                    ),
                )
                .when(!on || !complete, |b| b.invisible())
                .tooltip(ui::tooltip(t!("check.run")))
                .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
            );
        let form_rows = self.form_rows(&t, window, cx);

        div()
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
            .child(
                div()
                    .id("check-body")
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(12.))
                    .px(px(20.))
                    .py(px(16.))
                    .child(Self::line(
                        &t,
                        t!("check.enabled").into(),
                        div()
                            .id("check-on")
                            .cursor_pointer()
                            .child(ui::toggle(&t, on))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.on = !this.on;
                                cx.notify();
                            })),
                    ))
                    .children(
                        form_rows
                            .into_iter()
                            .map(|row| row.when(!on, |d| d.opacity(0.4))),
                    )
                    .child(
                        Self::line(
                            &t,
                            t!("check.every").into(),
                            div().id("every").rounded(px(8.)).bg(t.pill).child(seg),
                        )
                        .when(!on, |d| d.opacity(0.4)),
                    )
                    // 关掉时也留着这一行（变淡），开关来回切时卡片不变高矮。
                    .child(
                        Self::line(&t, t!("check.now").into(), now).when(!on, |d| d.opacity(0.4)),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(20.))
                    .h(px(56.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .child(div().flex_1())
                    .child(
                        ui::button("check-cancel", &t, Button::Secondary)
                            .child(t!("common.cancel"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(CheckEvent::Closed))),
                    )
                    .child(
                        ui::button("check-save", &t, Button::Primary)
                            .child(t!("common.save"))
                            .when(!complete, |b| b.opacity(0.4).cursor_default())
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}
