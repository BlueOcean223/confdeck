//! 设置页（顶栏的齿轮）：设一次就不动的东西，一块一张卡。
//!
//! - Claude Code：跳过首次确认、VS Code 插件跟着切换、隐藏 AI 署名（decisions「配置页」：一次性的设置放设置页，
//!   署名先做成全局开关）。点一下写一次（`Store::set_claude_toggle`），不攒着等保存。行里的小字是文件和键名，查问题时用。
//! - 备份（decisions「备份」）：右上角的下拉框选文件归谁（默认 Claude Code），只列这一家有备份的文件，
//!   一个文件一行（几份、多大、最近一次），点开列每一份的时间和原因，可以看和现在的差异、恢复、「清理」（删原件以外的）。

use std::path::PathBuf;

use chrono::NaiveDateTime;
use confdeck_core::agent::AgentKind;
use confdeck_core::backup::{Backup, Reason};
use confdeck_core::diff::{self, Row};
use confdeck_core::toggles::{CLAUDE_TOGGLES, ClaudeToggle};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{BackupOwner, FileBackups, Logo, Store};
use crate::theme::Theme;
use crate::ui;

/// 点开的「差异」：第几个文件的第几份，和算好的行。备份列表变了（恢复以后多一份）按路径找回来。
struct DiffView {
    file: usize,
    backup: usize,
    path: PathBuf,
    rows: Vec<Row>,
}

pub struct SettingsPage {
    store: Entity<Store>,
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
    _subscription: Subscription,
}

/// 差异里变了的地方前后各留几行。
const CONTEXT: usize = 3;

impl SettingsPage {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let _subscription = cx.observe(&store, |this, _, cx| {
            this.refresh(cx);
            cx.notify();
        });
        let files = store.read(cx).backups();
        let owners = store.read(cx).backup_owners();
        // 默认看 Claude Code 的（用户 10-12）。
        let owner = owners
            .iter()
            .find(|owner| owner.id == AgentKind::ClaudeCode.id())
            .or(owners.first())
            .map_or("", |owner| owner.id);
        Self {
            store,
            files,
            owners,
            owner,
            menu: false,
            just_closed: false,
            expanded: None,
            diff: None,
            _subscription,
        }
    }

    /// 文件写了（开关、恢复、清理）以后重列备份；点开的差异按新内容重算。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.files = self.store.read(cx).backups();
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

    fn flip(&mut self, toggle: ClaudeToggle, cx: &mut Context<Self>) {
        let on = self.store.read(cx).claude_toggle(toggle);
        self.store
            .update(cx, |store, cx| store.set_claude_toggle(toggle, !on, cx));
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

    fn title(&self, t: &Theme) -> Div {
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .h(px(28.))
            .px(px(4.))
            .text_size(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.fg)
            .child(t!("settings.title"))
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
            let on = store.claude_toggle(toggle);
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
                    .on_click(cx.listener(move |this, _, _, cx| this.flip(toggle, cx)))
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
            .child(self.title(&t))
            .child(self.claude_card(&t, window, cx))
            .child(self.backups_card(&t, window, cx))
    }
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
