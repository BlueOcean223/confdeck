//! 设置页（顶栏的齿轮）：设一次就不动的东西，一块一张卡，分成三段：通用、Claude Code、数据。
//!
//! - 通用 › 显示哪些 agent：每个 agent 两个开关，Agent 页一个、菜单栏面板一个（`Store::set_agent_hidden`，记在 `state.json`）。
//!   默认都开；关掉只是不列，配置文件和选择都不动。
//! - 通用 › 出站代理（默认跟随系统，可以单独设置）：跟随系统 / 不用代理 / 自定义，
//!   记在 `state.json`；「测试」连一次 models.dev。confdeck 自己联网（测连通、拉模型列表、models.dev）都走它。
//! - Claude Code：跳过首次确认、VS Code 插件跟着切换、隐藏 AI 署名（一次性的设置放设置页，
//!   署名先做成全局开关）。点一下写一次（`Store::set_claude_toggle`），不攒着等保存。行里的小字是文件和键名，查问题时用。
//!   开关的状态打开设置页、点了开关以后各读一次文件，画的时候只看读好的（悬停、动画都会重画）。
//! - 数据 › 导入导出：供应商导出成 `providers.json` 格式的文件（可以不带 Key），从这样的文件导入（先列出新增、覆盖哪几家再确认）。
//! - 数据 › 备份：右上角的下拉框选文件归谁（默认 Claude Code），只列这一家有备份的文件，
//!   一个文件一行（几份、多大、最近一次），点开列每一份的时间和原因，可以看和现在的差异、恢复、「清理」（删原件以外的）。

use std::path::PathBuf;
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use confdeck_core::agent::AgentKind;
use confdeck_core::backup::{Backup, Reason};
use confdeck_core::diff::{self, Row};
use confdeck_core::engine::Place;
use confdeck_core::net::{self as core_net, NetError, Proxy, ProxyMode};
use confdeck_core::provider::Provider;
use confdeck_core::toggles::{CLAUDE_TOGGLES, ClaudeToggle};
use confdeck_core::transfer::Plan;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{BackupOwner, FileBackups, Logo, Store};
use crate::errors::Describe as _;
use crate::theme::Theme;
use crate::ui::{self, Button};
use crate::{files, net};

/// 点开的「差异」：第几个文件的第几份，和算好的行。备份列表变了（恢复以后多一份）按路径找回来。
struct DiffView {
    file: usize,
    backup: usize,
    path: PathBuf,
    rows: Vec<Row>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Claude,
    Data,
}

enum ProxyTest {
    Testing,
    Done(Result<Duration, NetError>),
}

/// 选好了要导入的文件，等确认。
struct PendingImport {
    incoming: Vec<Provider>,
    plan: Plan,
}

pub struct SettingsPage {
    store: Entity<Store>,
    section: Section,
    /// 分段标签的滑块从哪一段滑过来、第几次切换（同顶栏）。
    section_from: Option<(usize, u64)>,
    switches: u64,
    /// 存着的出站代理；自定义的地址在输入框里改，写对了就存。
    proxy: Proxy,
    proxy_url: Entity<InputState>,
    /// 「跟随系统」现在走的代理，打开设置页时读一次。
    system_proxy: Option<String>,
    proxy_test: Option<ProxyTest>,
    /// 自定义的地址写得不对。
    proxy_error: Option<SharedString>,
    /// 输入框里上一次的内容：回车时输入框会再发一次「改了」，内容没变，不能把刚报的错清掉。
    proxy_seen: String,
    /// Claude Code 的三个开关（和 `CLAUDE_TOGGLES` 一一对应）。打开设置页、点了开关以后各读一次，画的时候不读文件。
    claude_toggles: Vec<bool>,
    /// 导出带不带 Key。
    export_keys: bool,
    import: Option<PendingImport>,
    /// 有备份的文件，各家的都在；画的时候只取 `owner` 那家的。
    files: Vec<FileBackups>,
    owners: Vec<BackupOwner>,
    /// 备份卡现在看的是谁的文件（`BackupOwner::id`）。
    owner: &'static str,
    /// 选文件归谁的下拉框开着。
    menu: bool,
    /// 下拉框刚被「点外面」关掉。点的正好是它的按钮时，不要立刻再打开。
    just_closed: bool,
    /// 展开的文件（`files` 的下标）。
    expanded: Option<usize>,
    diff: Option<DiffView>,
    /// 上次重列备份时 Store 换上过几份快照（`Store::generation`）。
    seen: u64,
    _subscriptions: Vec<Subscription>,
}

/// 差异里变了的地方前后各留几行。
const CONTEXT: usize = 3;

/// 「显示哪些 agent」卡里一列开关的宽度（列名也按这个宽度居中）。
const COL: f32 = 72.;

const SECTIONS: [Section; 3] = [Section::General, Section::Claude, Section::Data];

impl SettingsPage {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Store 换上新快照（写了文件、重读过）才重列备份、重读开关；额度查回来这类通知和文件无关。
        let _subscription = cx.observe(&store, |this, store, cx| {
            let generation = store.read(cx).generation();
            if generation != this.seen {
                this.seen = generation;
                this.refresh(cx);
            }
            cx.notify();
        });
        let proxy = store.read(cx).proxy();
        let proxy_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("http://127.0.0.1:7890")
                .default_value(proxy.url.clone())
        });
        let proxy_seen = proxy.url.clone();
        let proxy_subscription = cx.subscribe_in(
            &proxy_url,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_url(window, cx),
                InputEvent::Change => this.url_changed(cx),
                _ => {}
            },
        );
        let files = store.read(cx).backups();
        let owners = store.read(cx).backup_owners();
        let claude_toggles = read_toggles(&store, cx);
        let seen = store.read(cx).generation();
        // 默认看 Claude Code 的。
        let owner = owners
            .iter()
            .find(|owner| owner.id == AgentKind::ClaudeCode.id())
            .or(owners.first())
            .map_or("", |owner| owner.id);
        Self {
            store,
            section: Section::General,
            section_from: None,
            switches: 0,
            proxy,
            proxy_url,
            system_proxy: core_net::system_proxy(),
            proxy_test: None,
            proxy_error: None,
            proxy_seen,
            claude_toggles,
            export_keys: true,
            import: None,
            files,
            owners,
            owner,
            menu: false,
            just_closed: false,
            expanded: None,
            diff: None,
            seen,
            _subscriptions: vec![_subscription, proxy_subscription],
        }
    }

    fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section == section {
            return;
        }
        let from = SECTIONS
            .iter()
            .position(|s| *s == self.section)
            .unwrap_or(0);
        self.switches += 1;
        self.section_from = Some((from, self.switches));
        self.section = section;
        self.menu = false;
        cx.notify();
    }

    fn save_proxy(&mut self, cx: &mut Context<Self>) {
        let proxy = self.proxy.clone();
        self.store
            .update(cx, |store, cx| store.set_proxy(proxy, cx));
        self.proxy_test = None;
        cx.notify();
    }

    fn set_proxy_mode(&mut self, mode: ProxyMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.proxy.mode == mode {
            return;
        }
        self.proxy.mode = mode;
        self.proxy_error = None;
        self.save_proxy(cx);
        if mode == ProxyMode::Custom && self.proxy.url.is_empty() {
            self.proxy_url
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// 边打边存：写对了就存（不改输入框里的写法），没写完、写错了先不管，回车或测试时再报。
    /// 点空白处输入框不会失焦，只靠回车存的话，打完直接关掉设置页就丢了。
    fn url_changed(&mut self, cx: &mut Context<Self>) {
        let typed = self.proxy_url.read(cx).value().to_string();
        if typed == self.proxy_seen {
            return;
        }
        self.proxy_seen = typed.clone();
        self.proxy_error = None;
        self.proxy_test = None;
        let url = Proxy::normalized_url(&typed);
        if url != self.proxy.url && (url.is_empty() || Proxy::check_url(&url).is_ok()) {
            self.proxy.url = url;
            self.save_proxy(cx);
        }
        cx.notify();
    }

    /// 回车、离开输入框、点测试：整理写法、校验、存。写得不对就不存，提示在测试那一格。
    fn commit_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.proxy_url.read(cx).value().to_string();
        let url = Proxy::normalized_url(&typed);
        if !url.is_empty() && Proxy::check_url(&url).is_err() {
            self.proxy_error = Some(t!("net.bad_proxy").into());
            cx.notify();
            return;
        }
        if url != typed {
            self.proxy_seen = url.clone();
            self.proxy_url
                .update(cx, |input, cx| input.set_value(url.clone(), window, cx));
        }
        if url != self.proxy.url {
            self.proxy.url = url;
            self.save_proxy(cx);
        }
    }

    fn test_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.proxy.mode == ProxyMode::Custom {
            self.commit_url(window, cx);
            if self.proxy_error.is_some() {
                return;
            }
        }
        self.proxy_test = Some(ProxyTest::Testing);
        let proxy = self.proxy.clone();
        let task = self
            .store
            .update(cx, |store, cx| store.test_proxy(proxy, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.proxy_test = Some(ProxyTest::Done(result));
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let keys = self.export_keys;
        let home = self.store.read(cx).home.clone();
        let downloads = home.join("Downloads");
        let dir = if downloads.is_dir() { downloads } else { home };
        let name = format!(
            "confdeck-providers-{}.json",
            Local::now().format("%Y-%m-%d")
        );
        let pick = files::pick_save(&dir, &name, cx);
        let store = self.store.clone();
        cx.spawn(async move |_, cx| {
            if let Some(path) = pick.await {
                store.update(cx, |store, cx| store.export(&path, keys, cx));
            }
        })
        .detach();
    }

    fn choose_import(&mut self, cx: &mut Context<Self>) {
        let pick = files::pick_open(cx);
        cx.spawn(async move |this, cx| {
            let Some(path) = pick.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let read = this.store.read(cx).read_import(&path);
                match read {
                    Ok((_, plan)) if plan.changes() == 0 => {
                        this.import = None;
                        this.store.update(cx, |store, cx| {
                            store.announce(
                                &[crate::data::Notice::ok(t!("transfer.nothing_new"))],
                                cx,
                            )
                        });
                    }
                    Ok((incoming, plan)) => this.import = Some(PendingImport { incoming, plan }),
                    Err(error) => {
                        this.import = None;
                        this.store.update(cx, |store, cx| {
                            store.announce(
                                &[crate::data::Notice::err(t!(
                                    "transfer.import_failed",
                                    error = error
                                ))],
                                cx,
                            )
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_import(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.import.take() {
            self.store
                .update(cx, |store, cx| store.import(pending.incoming, cx));
        }
        cx.notify();
    }

    /// 文件写了（开关、恢复、清理、别处切换）以后重列备份、重读开关；点开的差异按新内容重算。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.files = self.store.read(cx).backups();
        self.claude_toggles = read_toggles(&self.store, cx);
        if self.expanded.is_some_and(|n| n >= self.files.len()) {
            self.expanded = None;
        }
        if let Some(view) = self.diff.take() {
            let found = self.files.iter().enumerate().find_map(|(n, entry)| {
                let m = entry
                    .backups
                    .iter()
                    .position(|backup| backup.path == view.path)?;
                Some((n, m))
            });
            if let Some((file, backup)) = found
                && self.expanded == Some(file)
            {
                self.open_diff(file, backup, cx);
            }
        }
    }

    /// 点了一个开关：先显示成点过的样子，写完（快照换上）再按文件重读。
    fn flip(&mut self, n: usize, cx: &mut Context<Self>) {
        let toggle = CLAUDE_TOGGLES[n];
        // 按显示的翻：上一次点的还没写完时文件里还是旧的。
        let on = self.claude_toggles[n];
        self.store
            .update(cx, |store, cx| store.set_claude_toggle(toggle, !on, cx));
        self.claude_toggles[n] = !on;
        cx.notify();
    }

    fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        if self.just_closed {
            return;
        }
        self.menu = !self.menu;
        cx.notify();
    }

    fn pick_owner(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if self.owner != id {
            self.owner = id;
            self.expanded = None;
            self.diff = None;
        }
        self.menu = false;
        cx.notify();
    }

    fn toggle_file(&mut self, n: usize, cx: &mut Context<Self>) {
        if self.expanded == Some(n) {
            self.expanded = None;
        } else {
            self.expanded = Some(n);
        }
        self.diff = None;
        cx.notify();
    }

    fn open_diff(&mut self, file: usize, backup: usize, cx: &mut Context<Self>) {
        if self
            .diff
            .as_ref()
            .is_some_and(|view| view.file == file && view.backup == backup)
        {
            self.diff = None;
            cx.notify();
            return;
        }
        let Some(entry) = self.files.get(file) else {
            return;
        };
        let Some(chosen) = entry.backups.get(backup) else {
            return;
        };
        let store = self.store.read(cx);
        let old = store.read_backup(chosen);
        let new = store.read_text(&entry.file);
        self.diff = Some(DiffView {
            file,
            backup,
            path: chosen.path.clone(),
            rows: diff::rows(&old, &new, CONTEXT),
        });
        cx.notify();
    }

    fn restore(&mut self, file: usize, backup: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.files.get(file) else {
            return;
        };
        let Some(chosen) = entry.backups.get(backup) else {
            return;
        };
        let (live, chosen) = (entry.file.clone(), chosen.clone());
        // 写完快照换上时重列备份、重读开关（恢复的可能就是开关所在的文件）。
        self.store
            .update(cx, |store, cx| store.restore(&live, &chosen, cx));
    }

    fn clean(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.files.get(file) else {
            return;
        };
        let live = entry.file.clone();
        self.store.update(cx, |store, cx| store.clean(&live, cx));
        self.refresh(cx);
        cx.notify();
    }

    fn title(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let sections: Vec<(Section, SharedString)> = vec![
            (Section::General, t!("settings.general").into()),
            (Section::Claude, "Claude Code".into()),
            (Section::Data, t!("settings.data").into()),
        ];
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .h(px(28.))
            .px(px(4.))
            .child(
                div()
                    .flex_1()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.fg)
                    .child(t!("settings.title")),
            )
            .child(
                div()
                    .id("sections")
                    .bg(t.pill)
                    .rounded(px(8.))
                    .child(ui::seg(
                        t,
                        &sections,
                        self.section,
                        self.section_from,
                        window,
                        cx.listener(|this, section: &Section, _, cx| this.show(*section, cx)),
                    )),
            )
    }

    fn proxy_card(&self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let modes: Vec<(ProxyMode, SharedString)> = vec![
            (ProxyMode::System, t!("proxy.system").into()),
            (ProxyMode::Direct, t!("proxy.direct").into()),
            (ProxyMode::Custom, t!("proxy.custom").into()),
        ];
        let header = ui::card_title(t, t!("proxy.title"))
            .child(div().flex_1())
            .child(
                div()
                    .id("proxy-mode")
                    .font_weight(FontWeight::NORMAL)
                    .bg(t.pill)
                    .rounded(px(8.))
                    .child(ui::seg(
                        t,
                        &modes,
                        self.proxy.mode,
                        None,
                        window,
                        cx.listener(|this, mode: &ProxyMode, window, cx| {
                            this.set_proxy_mode(*mode, window, cx)
                        }),
                    )),
            );
        let small = |text: SharedString, color: Hsla| {
            div()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .text_color(color)
                .child(text)
        };
        let left = match self.proxy.mode {
            ProxyMode::System => match &self.system_proxy {
                Some(url) => div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .child(small(t!("proxy.system_is").into(), t.muted).flex_none())
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family("Menlo")
                            .text_size(px(12.))
                            .child(url.clone()),
                    ),
                None => small(t!("proxy.no_system").into(), t.muted),
            },
            ProxyMode::Direct => small(t!("proxy.direct_note").into(), t.muted),
            ProxyMode::Custom => {
                div()
                    .w(px(320.))
                    .child(ui::field(t, &self.proxy_url, true, None, window, cx))
            }
        };
        let result = match (&self.proxy_error, &self.proxy_test) {
            (Some(error), _) => Some(small(error.clone(), t.danger)),
            (None, Some(ProxyTest::Testing)) => Some(small(t!("proxy.testing").into(), t.muted)),
            (None, Some(ProxyTest::Done(Ok(duration)))) => Some(small(
                t!("proxy.ok", latency = net::latency(*duration)).into(),
                t.success,
            )),
            (None, Some(ProxyTest::Done(Err(error)))) => {
                Some(small(error.describe().into(), t.danger))
            }
            (None, None) => None,
        };
        ui::list(t, false).child(header).child(
            div()
                .h_flex()
                .items_center()
                .gap(px(12.))
                .h(px(48.))
                .pl(px(14.))
                .pr(px(12.))
                .border_t_1()
                .border_color(t.line_2)
                .child(div().flex_1().min_w_0().child(left))
                .children(result)
                .child(
                    ui::text_button("proxy-test", t)
                        .flex_none()
                        .child(t!("proxy.test"))
                        .on_click(cx.listener(|this, _, window, cx| this.test_proxy(window, cx))),
                ),
        )
    }

    /// 导入导出：两行，导出（带不带 Key）、导入（选了文件先列出会改什么，确认了才导）。
    fn transfer_card(&self, t: &Theme, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let count = store.providers.len();
        let row = || {
            div()
                .h_flex()
                .items_center()
                .gap(px(12.))
                .min_h(px(52.))
                .py(px(8.))
                .pl(px(14.))
                .pr(px(12.))
                .border_t_1()
                .border_color(t.line_2)
        };
        let title = |text: SharedString, sub: Option<SharedString>| {
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(12.5)).child(text))
                .children(sub.map(|sub| {
                    div()
                        .mt(px(2.))
                        .text_size(px(11.))
                        .text_color(t.muted)
                        .child(sub)
                }))
        };
        let keys = self.export_keys;
        let export = row()
            .child(title(
                t!("transfer.export").into(),
                Some(if count == 1 {
                    t!("providers.count_one").into()
                } else {
                    t!("providers.count_other", count = count).into()
                }),
            ))
            .child(
                div()
                    .id("export-keys")
                    .h_flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .child(ui::checkbox(t, keys))
                    .child(div().text_size(px(12.)).child(t!("transfer.with_keys")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.export_keys = !keys;
                        cx.notify();
                    })),
            )
            .child(
                ui::button("export", t, Button::Secondary)
                    .child(t!("transfer.export_button"))
                    .when(count > 0 && store.load_error.is_none(), |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.export(cx)))
                    })
                    .when(count == 0 || store.load_error.is_some(), |b| {
                        b.opacity(0.5).cursor_default()
                    }),
            );
        let import = match &self.import {
            None => row()
                .child(title(t!("transfer.import").into(), None))
                .child(
                    ui::button("import", t, Button::Secondary)
                        .child(t!("transfer.import_button"))
                        .when(store.load_error.is_none(), |b| {
                            b.on_click(cx.listener(|this, _, _, cx| this.choose_import(cx)))
                        })
                        .when(store.load_error.is_some(), |b| {
                            b.opacity(0.5).cursor_default()
                        }),
                ),
            Some(pending) => {
                let names = |list: &[String]| list.join(&t!("common.list_separator"));
                let mut parts: Vec<String> = Vec::new();
                if !pending.plan.added.is_empty() {
                    parts.push(t!("transfer.adds", names = names(&pending.plan.added)).into());
                }
                if !pending.plan.replaced.is_empty() {
                    parts.push(
                        t!("transfer.replaces", names = names(&pending.plan.replaced)).into(),
                    );
                }
                let changes = pending.plan.changes();
                row()
                    .child(title(
                        t!("transfer.import").into(),
                        Some(parts.join(" · ").into()),
                    ))
                    .child(
                        ui::button("import-cancel", t, Button::Secondary)
                            .child(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.import = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::button("import-confirm", t, Button::Primary)
                            .child(if changes == 1 {
                                t!("transfer.import_one")
                            } else {
                                t!("transfer.import_other", count = changes)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_import(cx))),
                    )
            }
        };
        ui::list(t, false)
            .child(ui::card_title(t, t!("transfer.title")))
            .child(export)
            .child(import)
    }

    fn agents_card(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let column = |text: SharedString| {
            div()
                .flex_none()
                .w(px(COL))
                .text_center()
                .text_size(px(11.))
                .font_weight(FontWeight::NORMAL)
                .text_color(t.muted)
                .child(text)
        };
        let mut card = ui::list(t, false).child(
            div()
                .h_flex()
                .items_center()
                .h(px(42.))
                .pl(px(14.))
                .pr(px(12.))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child(div().flex_1().child(t!("settings.agents")))
                .child(column(t!("settings.on_page").into()))
                .child(column(t!("settings.in_panel").into())),
        );
        for (i, agent) in store.agents.iter().enumerate() {
            let switch = |place: Place, on: bool, name: &'static str| {
                div()
                    .id(ElementId::named_usize(name, i))
                    .flex_none()
                    .w(px(COL))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |store, cx| store.set_agent_hidden(i, place, on, cx))
                    }))
                    .child(ui::toggle(t, on))
            };
            card = card.child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(44.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .child(ui::logo(agent.logo, 16., t, window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.5))
                            .truncate()
                            .child(agent.name),
                    )
                    .child(switch(Place::Page, !agent.hidden_on_page, "page"))
                    .child(switch(Place::Panel, !agent.hidden_in_panel, "panel")),
            );
        }
        card
    }

    fn claude_card(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let logo = store
            .agents
            .iter()
            .find(|agent| agent.name == "Claude Code")
            .map(|agent| agent.logo)
            .unwrap_or(Logo::Letter('C'));
        let mut card = ui::list(t, false).child(
            div()
                .h_flex()
                .items_center()
                .gap(px(8.))
                .h(px(42.))
                .pl(px(14.))
                .pr(px(12.))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child(ui::logo(logo, 16., t, window, cx))
                .child("Claude Code"),
        );
        for (n, &toggle) in CLAUDE_TOGGLES.iter().enumerate() {
            let on = self.claude_toggles[n];
            let hover = t.card_2;
            card = card.child(
                div()
                    .id(ElementId::named_usize("toggle", n))
                    .h_flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(48.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .cursor_pointer()
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.flip(n, cx)))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(12.5)).child(label(toggle)))
                            .child(
                                div()
                                    .mt(px(2.))
                                    .truncate()
                                    .font_family("Menlo")
                                    .text_size(px(10.5))
                                    .text_color(t.faint)
                                    .child(format!(
                                        "{} · {}",
                                        store.claude_toggle_path(toggle),
                                        toggle.key()
                                    )),
                            ),
                    )
                    .child(ui::toggle(t, on)),
            );
        }
        card
    }

    fn backups_card(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let owner = self
            .owners
            .iter()
            .find(|owner| owner.id == self.owner)
            .copied();
        let mut card = ui::list(t, false).child(
            ui::card_title(t, t!("settings.backups"))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::NORMAL)
                        .text_size(px(11.5))
                        .text_color(t.muted)
                        .child(store.tilde(&store.backups_dir())),
                )
                .child(div().flex_1())
                .when_some(owner, |row, owner| {
                    row.child(
                        div()
                            .relative()
                            .flex_none()
                            .w(px(150.))
                            .font_weight(FontWeight::NORMAL)
                            .child(
                                ui::select_pill(
                                    "backup-owner",
                                    t,
                                    false,
                                    &[owner.logo],
                                    owner.name,
                                    self.menu,
                                    false,
                                    window,
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(cx))),
                            )
                            .when(self.menu, |d| {
                                d.child(ui::dropdown(self.owner_menu(t, window, cx)))
                            }),
                    )
                }),
        );
        let shown: Vec<usize> = (0..self.files.len())
            .filter(|&n| self.files[n].file.owner == self.owner)
            .collect();
        if shown.is_empty() {
            return card.child(
                div()
                    .h_flex()
                    .items_center()
                    .h(px(48.))
                    .px(px(14.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .child(t!("settings.no_backups")),
            );
        }
        for n in shown {
            let entry = &self.files[n];
            let open = self.expanded == Some(n);
            let hover = t.card_2;
            let total: u64 = entry.backups.iter().map(|backup| backup.size).sum();
            let latest = entry
                .backups
                .first()
                .map(|backup| time_text(backup.time))
                .unwrap_or_default();
            card = card.child(
                div()
                    .id(ElementId::named_usize("file", n))
                    .h_flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(48.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .cursor_pointer()
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(n, cx)))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .child(store.tilde(&entry.file.path)),
                            )
                            .child(
                                div()
                                    .mt(px(2.))
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(t.muted)
                                    .child(format!(
                                        "{} · {} · {latest}",
                                        count_text(entry.backups.len()),
                                        size_text(total)
                                    )),
                            ),
                    )
                    .child(ui::icon(
                        if open { "chevron-up" } else { "chevron-down" },
                        13.,
                        t.faint,
                    )),
            );
            if !open {
                continue;
            }
            for (m, backup) in entry.backups.iter().enumerate() {
                let showing = self
                    .diff
                    .as_ref()
                    .is_some_and(|view| view.file == n && view.backup == m);
                card = card.child(self.backup_row(n, m, backup, showing, t, cx));
                if showing && let Some(view) = &self.diff {
                    card = card.child(self.diff_box(&view.rows, t));
                }
            }
            if entry.cleanable() > 0 {
                card = card.child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_end()
                        .h(px(40.))
                        .pl(px(28.))
                        .pr(px(12.))
                        .border_t_1()
                        .border_color(t.line_2)
                        .child(
                            ui::text_button(ElementId::named_usize("clean", n), t)
                                .child(t!("settings.clean"))
                                .on_click(cx.listener(move |this, _, _, cx| this.clean(n, cx))),
                        ),
                );
            }
        }
        card
    }

    /// 文件归谁的下拉框：接上的 agent 和 confdeck 自己，现在看的那家打勾。
    fn owner_menu(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let hover = t.sel;
        let mut list = div().v_flex().p(px(5.));
        for (n, owner) in self.owners.iter().enumerate() {
            let id = owner.id;
            list = list.child(
                div()
                    .id(ElementId::named_usize("owner-option", n))
                    .h_flex()
                    .items_center()
                    .gap(px(9.))
                    .h(px(32.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_owner(id, cx)))
                    .child(ui::logo(owner.logo, 14., t, window, cx))
                    .child(div().flex_1().text_size(px(12.)).child(owner.name))
                    .when(id == self.owner, |row| {
                        row.child(ui::icon("check", 13., t.accent))
                    }),
            );
        }
        div()
            .v_flex()
            .w(px(200.))
            .bg(Hsla { a: 1., ..t.pop_bg })
            .rounded(px(11.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .overflow_hidden()
            .occlude()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = false;
                this.just_closed = true;
                cx.notify();
            }))
            .child(list)
    }

    /// 一份备份：时间、原因、大小，右边「差异」「恢复」。
    fn backup_row(
        &self,
        n: usize,
        m: usize,
        backup: &Backup,
        showing: bool,
        t: &Theme,
        cx: &Context<Self>,
    ) -> Div {
        let id = n * 1000 + m;
        div()
            .h_flex()
            .items_center()
            .gap(px(12.))
            .h(px(36.))
            .pl(px(28.))
            .pr(px(12.))
            .border_t_1()
            .border_color(t.line_2)
            .child(
                div()
                    .flex_none()
                    .font_family("Menlo")
                    .text_size(px(11.5))
                    .text_color(t.fg_2)
                    .child(time_text(backup.time)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .child(reason_text(backup.reason)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(t.faint)
                    .child(size_text(backup.size)),
            )
            .child(div().flex_1())
            .child(
                ui::text_button(ElementId::named_usize("diff", id), t)
                    .when(showing, |b| b.text_color(t.fg))
                    .child(t!("settings.diff"))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_diff(n, m, cx))),
            )
            .child(
                ui::text_button(ElementId::named_usize("restore", id), t)
                    .child(t!("settings.restore"))
                    .on_click(cx.listener(move |this, _, _, cx| this.restore(n, m, cx))),
            )
    }

    /// 备份和现在的文件的差异：删了的红、加了的绿，没变的一长段折成「⋯ N 行」。
    fn diff_box(&self, rows: &[Row], t: &Theme) -> Div {
        let changed = rows
            .iter()
            .any(|row| matches!(row, Row::Added(_) | Row::Removed(_)));
        let mut body = div().v_flex().py(px(6.));
        if !changed {
            body = body.child(
                div()
                    .h(px(22.))
                    .px(px(12.))
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(t!("settings.same_as_now")),
            );
        }
        for (i, row) in rows.iter().enumerate() {
            if !changed {
                break;
            }
            let line = div()
                .id(ElementId::named_usize("diff-row", i))
                .h_flex()
                .items_center()
                .min_h(px(20.))
                .px(px(12.))
                .gap(px(8.))
                .font_family("Menlo")
                .text_size(px(11.))
                .whitespace_nowrap();
            body = body.child(match row {
                Row::Same(text) => line
                    .text_color(t.fg_2)
                    .child(div().w(px(8.)).flex_none())
                    .child(text.clone()),
                Row::Added(text) => line
                    .bg(Hsla {
                        a: 0.14,
                        ..t.success
                    })
                    .text_color(t.fg)
                    .child(div().w(px(8.)).flex_none().text_color(t.success).child("+"))
                    .child(text.clone()),
                Row::Removed(text) => line
                    .bg(Hsla {
                        a: 0.12,
                        ..t.danger
                    })
                    .text_color(t.fg)
                    .child(div().w(px(8.)).flex_none().text_color(t.danger).child("−"))
                    .child(text.clone()),
                Row::Skipped(count) => line
                    .text_color(t.faint)
                    .child(div().w(px(8.)).flex_none())
                    .child(skipped_text(*count)),
            });
        }
        div()
            .mx(px(14.))
            .mb(px(10.))
            .rounded(px(8.))
            .bg(t.field)
            .border_1()
            .border_color(t.line)
            .overflow_hidden()
            .child(
                div()
                    .id("diff")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .child(body),
            )
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        div()
            .v_flex()
            .gap(px(10.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = false),
            )
            .child(self.title(&t, window, cx))
            .map(|page| match self.section {
                Section::General => page
                    .child(self.agents_card(&t, window, cx))
                    .child(self.proxy_card(&t, window, cx)),
                Section::Claude => page.child(self.claude_card(&t, window, cx)),
                Section::Data => page
                    .child(self.transfer_card(&t, cx))
                    .child(self.backups_card(&t, window, cx)),
            })
    }
}

/// 三个开关现在是开是关（读文件）。
fn read_toggles(store: &Entity<Store>, cx: &App) -> Vec<bool> {
    let store = store.read(cx);
    CLAUDE_TOGGLES
        .iter()
        .map(|&toggle| store.claude_toggle(toggle))
        .collect()
}

fn label(toggle: ClaudeToggle) -> SharedString {
    match toggle {
        ClaudeToggle::SkipOnboarding => t!("settings.skip_onboarding").into(),
        ClaudeToggle::VscodeFollows => t!("settings.vscode_follows").into(),
        ClaudeToggle::HideAttribution => t!("settings.hide_attribution").into(),
    }
}

fn reason_text(reason: Reason) -> SharedString {
    match reason {
        Reason::Original => t!("backup.original").into(),
        Reason::External => t!("backup.external").into(),
        Reason::BeforeEdit => t!("backup.before_edit").into(),
        Reason::BeforeRestore => t!("backup.before_restore").into(),
        Reason::BeforeSave => t!("backup.before_save").into(),
    }
}

fn count_text(count: usize) -> SharedString {
    if count == 1 {
        t!("settings.count_one").into()
    } else {
        t!("settings.count_other", count = count).into()
    }
}

fn skipped_text(count: usize) -> SharedString {
    if count == 1 {
        t!("settings.skipped_one").into()
    } else {
        t!("settings.skipped_other", count = count).into()
    }
}

fn time_text(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `812 B`、`12 KB`、`1.3 MB`。
fn size_text(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < KB * KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    // 不 `use super::*`：gpui 的 glob 里有个 `test` 宏，会盖掉标准的 `#[test]`。
    use super::size_text;

    #[test]
    fn sizes_read_like_the_finder() {
        assert_eq!(size_text(812), "812 B");
        assert_eq!(size_text(12 * 1024 + 300), "12 KB");
        assert_eq!(size_text(1_400_000), "1.3 MB");
    }
}
