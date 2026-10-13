//! 用户点出来的写操作都走 [`Store::run`]：调的人先改好内存（界面马上显示），core 的写操作在后台线程跑，
//! 同一个后台任务里接着读一份快照；回到界面线程换上快照，把结果（[`Report`]）翻成提示发出去。
//!
//! 写和重读排在同一队里（`Store::enqueue`），一个跑完才开始下一个：快照读不到写了一半的。
//! 后面还排着的，这一份快照就不换上（最后那份读得到前面所有的改动），免得先改的内存被盖回去又改过来。
//! 所以先改内存的那一步要把这次写的结果都放进去：后面排着的写拿的是排队时的内存（整份供应商列表交给 core 去存），
//! 漏了的会被它写回旧值。
//!
//! 这里是供应商、agent 文件、开关、备份的写操作；切官方账号在 `accounts.rs`，导入在 `net.rs`，也走 `run`。

use std::borrow::Cow;
use std::path::Path;

use confdeck_core::agent::codex::catalog::Skipped;
use confdeck_core::agent::{AgentKind, Mode};
use confdeck_core::apply::{self, Synced};
use confdeck_core::backup::Backup;
use confdeck_core::engine::{Engine, LiveFile, Outcome, Place};
use confdeck_core::provider::Provider;
use confdeck_core::status::AgentStatus;
use confdeck_core::store::StoreError;
use confdeck_core::toggles::ClaudeToggle;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Notice, Store, file_name};

/// 一次写操作做的是什么：[`Report::notices`] 照它挑头一句、整个没写成时那句。
pub(crate) enum Done {
    /// 单选 agent 换了一家（`None` 是「默认」），或换了官方账号（名字是账号的名字或邮箱）。
    Switched {
        agent: AgentKind,
        to: Option<String>,
    },
    /// 多选 agent 勾上、取消一家。
    Toggled {
        agent: AgentKind,
        provider: String,
        on: bool,
    },
    /// 把 agent 文件里的 Key 存进了这家。
    KeyAdopted {
        agent: AgentKind,
        provider: String,
    },
    /// 改了这个 agent 的配置（「配置 ›」页、设置页的开关）：它的文件真变了才说。
    Updated {
        agent: AgentKind,
    },
    /// 编辑器存了、删了一家。没写成的原因编辑器自己显示在底栏，不做成提示。
    Saved {
        provider: String,
    },
    Deleted {
        provider: String,
    },
    /// 导入换了几家。
    Imported {
        count: usize,
    },
    /// 用备份替换了这个文件（文件名）。
    Restored {
        file: String,
    },
    /// 在 Agent 页或面板里藏起、显示一个 agent：只改 `state.json`，写成了不用说。
    Hidden {
        agent: AgentKind,
    },
}

/// 一次写操作的结果。
pub(crate) struct Report {
    done: Done,
    /// 整个没写成的原因（已经是界面上的话）。
    pub(crate) failed: Option<String>,
    /// 写了文件的 agent：改了一家供应商，在用它的都跟着写。
    written: Vec<AgentKind>,
    /// 跟着写、没写成的 agent 和原因。
    unwritten: Vec<(AgentKind, String)>,
    /// Codex 的模型目录没写成：config.toml 已经切过去了，但 `/model` 列的还是 GPT 模型，要说一声。
    catalog: Option<Skipped>,
}

impl Report {
    pub(crate) fn new(done: Done) -> Self {
        Self {
            done,
            failed: None,
            written: Vec::new(),
            unwritten: Vec::new(),
            catalog: None,
        }
    }

    pub(crate) fn fail(mut self, error: impl Into<String>) -> Self {
        self.failed = Some(error.into());
        self
    }

    /// core 跟着写各 agent 的结果（`changed` 为假是没动它的文件）。
    pub(crate) fn synced(mut self, synced: Vec<Synced>) -> Self {
        for entry in synced {
            match entry.result {
                Ok(outcome) => self = self.wrote(entry.agent, &outcome),
                Err(err) => self.unwritten.push((entry.agent, err.to_string())),
            }
        }
        self
    }

    /// 写了这个 agent 的一个文件。
    pub(crate) fn wrote(mut self, agent: AgentKind, outcome: &Outcome) -> Self {
        if outcome.changed && !self.written.contains(&agent) {
            self.written.push(agent);
        }
        self
    }

    fn catalog(mut self, catalog: Option<Result<Outcome, Skipped>>) -> Self {
        self.catalog = catalog.and_then(Result::err);
        self
    }

    /// 翻成提示：头一句（写成了做了什么，或者为什么没写成），再是没写成的模型目录、跟着写没写成的 agent，每个一条。
    pub(crate) fn notices(&self) -> Vec<Notice> {
        if let Some(error) = &self.failed {
            return self
                .done
                .failure(error)
                .map(Notice::err)
                .into_iter()
                .collect();
        }
        let mut notices: Vec<Notice> = self
            .done
            .headline(&self.written)
            .map(Notice::ok)
            .into_iter()
            .collect();
        if let Some(skipped) = self.catalog {
            notices.push(Notice::err(t!(
                "write.codex_catalog_skipped",
                reason = describe_skipped(skipped)
            )));
        }
        notices.extend(
            self.unwritten
                .iter()
                .map(|(agent, error)| Notice::err(sync_failed(*agent, error))),
        );
        notices
    }
}

impl Done {
    /// 写成了的那句。`written` 是写了文件的 agent。
    fn headline(&self, written: &[AgentKind]) -> Option<Cow<'static, str>> {
        let agents = || {
            written
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(&t!("common.list_separator"))
        };
        Some(match self {
            Done::Switched {
                agent,
                to: Some(to),
            } => {
                t!("write.switched", agent = agent.to_string(), provider = to)
            }
            Done::Switched { agent, to: None } => t!("write.restored", agent = agent.to_string()),
            Done::Toggled {
                agent,
                provider,
                on: true,
            } => t!(
                "write.enabled",
                agent = agent.to_string(),
                provider = provider
            ),
            Done::Toggled {
                agent,
                provider,
                on: false,
            } => t!(
                "write.disabled",
                agent = agent.to_string(),
                provider = provider
            ),
            Done::KeyAdopted { agent, provider } if written.is_empty() => t!(
                "write.key_adopted",
                provider = provider,
                agent = agent.to_string()
            ),
            Done::KeyAdopted { agent, provider } => t!(
                "write.key_adopted_synced",
                provider = provider,
                agent = agent.to_string(),
                agents = agents()
            ),
            // 别的在用这家的 agent 覆盖没动，文件不变；只说改的这个。
            Done::Updated { agent } if written.contains(agent) => {
                t!("write.updated", agent = agent.to_string())
            }
            Done::Updated { .. } | Done::Hidden { .. } => return None,
            Done::Saved { provider } if written.is_empty() => t!("toast.saved", name = provider),
            Done::Saved { .. } => t!("toast.saved_synced", agents = agents()),
            Done::Deleted { provider } if written.is_empty() => {
                t!("toast.deleted", name = provider)
            }
            Done::Deleted { .. } => t!("toast.deleted_synced", agents = agents()),
            Done::Imported { count: 1 } => t!("transfer.imported_one"),
            Done::Imported { count } => t!("transfer.imported_other", count = count),
            Done::Restored { file } => t!("write.file_restored", file = file),
        })
    }

    /// 整个没写成的那句。
    fn failure(&self, error: &str) -> Option<Cow<'static, str>> {
        Some(match self {
            Done::Switched { agent, .. }
            | Done::Toggled { agent, .. }
            | Done::Updated { agent }
            | Done::Hidden { agent } => sync_failed(*agent, error),
            Done::KeyAdopted { provider, .. } => {
                t!("write.key_adopt_failed", provider = provider, error = error)
            }
            Done::Saved { .. } | Done::Deleted { .. } => return None,
            Done::Imported { .. } => t!("transfer.import_failed", error = error),
            Done::Restored { .. } => error.to_string().into(),
        })
    }
}

fn sync_failed(agent: AgentKind, error: &str) -> Cow<'static, str> {
    t!(
        "toast.sync_failed",
        agent = agent.to_string(),
        error = error
    )
}

/// Codex 模型目录没生成出来的原因，按界面语言说（core 的 `Skipped` 只有中文）。
pub(crate) fn describe_skipped(skipped: Skipped) -> Cow<'static, str> {
    match skipped {
        Skipped::NotFound => t!("codex.not_found"),
        Skipped::Failed => t!("codex.failed"),
        Skipped::NoTemplate => t!("codex.no_template"),
    }
}

/// 一次写操作的结果，写完、提示发出去以后才有（后面还排着别的时，快照还没换上：换的是最后排的那份）。
/// 不等它也行：换快照、发提示都不靠它。
pub(crate) struct Written(Option<async_channel::Receiver<Report>>);

impl Written {
    /// 没什么可写（选的就是现在这家）。
    pub(crate) fn nothing() -> Self {
        Self(None)
    }

    /// 等写完。`None` 是没写（或者应用在退出）。
    pub(crate) async fn report(self) -> Option<Report> {
        self.0?.recv().await.ok()
    }
}

impl Store {
    /// 跑一次写操作（见本文件开头）。内存要先改的，调之前改好。
    pub(crate) fn run(
        &mut self,
        write: impl FnOnce(&Engine, &Path) -> Report + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Written {
        cx.notify();
        let (tell, told) = async_channel::bounded(1);
        self.enqueue(
            write,
            move |store, report: Report, cx| {
                store.announce(&report.notices(), cx);
                tell.try_send(report).ok();
            },
            cx,
        );
        Written(Some(told))
    }

    /// 单选 agent 换一家（`None` 是「默认」）。胶囊先改过来，不等写完；写失败了快照会把它退回文件里的实际状态。
    pub fn switch(
        &mut self,
        agent: usize,
        provider: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Written {
        let kind = self.agents[agent].kind;
        // 选的就是现在这家、文件是 confdeck 写的样子、state.json 也记着它：没什么可写，也不提示「已切到」。
        // 文件认出来是这家但 state 里没记（比如刚把文件里的连接存成供应商）还是要走一遍，把选择记下来。
        let picked = provider.map(|p| self.providers[p].clone());
        let status = match &picked {
            Some(p) => AgentStatus::Provider { id: p.id.clone() },
            None => AgentStatus::Official,
        };
        let picked_id = picked.as_ref().map(|p| p.id.as_str());
        let unchanged =
            self.local.selection(kind) == Some(picked_id) && self.agents[agent].status == status;
        if unchanged {
            return Written::nothing();
        }
        self.agents[agent].status = status;
        self.run(
            move |engine, home| {
                let report = Report::new(Done::Switched {
                    agent: kind,
                    to: picked.as_ref().map(|p| p.name.clone()),
                });
                match apply::switch(engine, home, kind, picked.as_ref()) {
                    Ok(switched) => report.catalog(switched.catalog),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 多选 agent 勾上或取消一家：勾选框先改，不等写完。
    pub fn toggle(&mut self, agent: usize, provider: usize, cx: &mut Context<Self>) -> Written {
        let kind = self.agents[agent].kind;
        if self.agents[agent].mode != Mode::Coexist {
            return Written::nothing();
        }
        let picked = self.providers[provider].clone();
        // 文件读不了时当成一家都没勾：照样去写，写的时候报哪一行坏了。
        let mut ids = match &self.agents[agent].status {
            AgentStatus::Multi { ids } => ids.clone(),
            _ => Vec::new(),
        };
        let on = !ids.contains(&picked.id);
        if on {
            ids.push(picked.id.clone());
        } else {
            ids.retain(|id| *id != picked.id);
        }
        self.agents[agent].status = AgentStatus::Multi { ids };
        self.run(
            move |engine, home| {
                let result = if on {
                    apply::enable(engine, home, kind, &picked)
                } else {
                    apply::disable(engine, home, kind, &picked.id)
                };
                let report = Report::new(Done::Toggled {
                    agent: kind,
                    provider: picked.name,
                    on,
                });
                match result {
                    Ok(_) => report,
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 「Key 不一致」的第二个选项：把这个 agent 配置文件里的 Key 存进那家供应商。
    /// 这个 agent 的文件本来就是这个 Key，不用写，只记下它选的是这家；别的在用这家的 agent
    /// 由 core 的 `save_provider` 跟着换成新 Key（core.md「改了供应商以后」）。
    pub fn adopt_key(
        &mut self,
        agent: usize,
        provider: usize,
        key: String,
        cx: &mut Context<Self>,
    ) -> Written {
        let kind = self.agents[agent].kind;
        let mut picked = self.providers[provider].clone();
        picked.key = key;
        self.agents[agent].status = AgentStatus::Provider {
            id: picked.id.clone(),
        };
        // `save_provider` 要拿改之前的列表认出在用这家的 agent。
        let before = self.providers.clone();
        self.providers[provider].key = picked.key.clone();
        self.run(
            move |engine, home| {
                let report = Report::new(Done::KeyAdopted {
                    agent: kind,
                    provider: picked.name.clone(),
                });
                let synced =
                    apply::save_provider(engine, home, &before, &picked).and_then(|synced| {
                        engine
                            .set_selection(kind.id(), Some(&picked.id))
                            .map_err(StoreError::Lock)?;
                        Ok(synced)
                    });
                match synced {
                    Ok(synced) => report.synced(synced),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 「配置 ›」页改了一项（槽位、列表、默认模型、接口）：`provider` 是改好覆盖的那家。
    /// 内存里先换掉，界面立刻显示新值；写了这个 agent 的文件就说一句「Claude Code 已更新」，没写成的单独报。
    pub fn save_override(
        &mut self,
        agent: usize,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Written {
        let kind = self.agents[agent].kind;
        // `save_provider` 要拿改之前的列表认出在用这家的 agent。
        let before = self.providers.clone();
        if let Some(slot) = self.providers.iter_mut().find(|p| p.id == provider.id) {
            *slot = provider.clone();
        }
        self.run(
            move |engine, home| {
                let report = Report::new(Done::Updated { agent: kind });
                match apply::save_provider(engine, home, &before, &provider) {
                    Ok(synced) => report.synced(synced),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 编辑器保存一家（新建或替换同 ID 的），写进在用它的 agent。列表先换上（存成供应商以后主窗口马上要按 ID 找它切过去）。
    pub fn save_provider(&mut self, provider: Provider, cx: &mut Context<Self>) -> Written {
        let before = self.providers.clone();
        match self.providers.iter_mut().find(|p| p.id == provider.id) {
            Some(slot) => *slot = provider.clone(),
            None => self.providers.push(provider.clone()),
        }
        self.run(
            move |engine, home| {
                let report = Report::new(Done::Saved {
                    provider: provider.name.clone(),
                });
                match apply::save_provider(engine, home, &before, &provider) {
                    Ok(synced) => report.synced(synced),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 编辑器删掉一家，多选 agent 里的那一项一起删。列表先拿掉它。
    pub fn delete_provider(&mut self, id: String, name: String, cx: &mut Context<Self>) -> Written {
        let before = self.providers.clone();
        self.providers.retain(|p| p.id != id);
        self.run(
            move |engine, home| {
                let report = Report::new(Done::Deleted { provider: name });
                match apply::delete_provider(engine, home, &before, &id) {
                    Ok(synced) => report.synced(synced),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        )
    }

    /// 设置页「显示哪些 agent」：在 Agent 页或菜单栏面板里显示 / 藏起一个 agent。记在 `state.json`。
    pub fn set_agent_hidden(
        &mut self,
        agent: usize,
        place: Place,
        hidden: bool,
        cx: &mut Context<Self>,
    ) {
        let kind = self.agents[agent].kind;
        match place {
            Place::Page => self.agents[agent].hidden_on_page = hidden,
            Place::Panel => self.agents[agent].hidden_in_panel = hidden,
        }
        self.run(
            move |engine, _| {
                let report = Report::new(Done::Hidden { agent: kind });
                match engine.set_hidden(place, kind.id(), hidden) {
                    Ok(_) => report,
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        );
    }

    /// 设置页点了一个 Claude Code 的开关：写那个文件（就改一个键），写了说一句「Claude Code 已更新」，没写成报错。
    pub fn set_claude_toggle(&mut self, toggle: ClaudeToggle, on: bool, cx: &mut Context<Self>) {
        self.run(
            move |engine, home| {
                let agent = AgentKind::ClaudeCode;
                let report = Report::new(Done::Updated { agent });
                match toggle.set(engine, home, on) {
                    Ok(outcome) => report.wrote(agent, &outcome),
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        );
    }

    /// 用一份备份替换文件（core 先把现在的内容备份成「恢复前」），说一句「已恢复 settings.json」。
    pub fn restore(&mut self, file: &LiveFile, backup: &Backup, cx: &mut Context<Self>) {
        let (file, backup) = (file.clone(), backup.clone());
        self.run(
            move |engine, _| {
                let report = Report::new(Done::Restored {
                    file: file_name(&file),
                });
                match engine.restore(&file, &backup) {
                    Ok(_) => report,
                    Err(err) => report.fail(err.to_string()),
                }
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;

    use confdeck_core::agent::AgentKind;
    use confdeck_core::agent::codex::catalog::Skipped;
    use confdeck_core::apply::{ApplyError, Synced};
    use confdeck_core::engine::Outcome;
    use rust_i18n::t;

    // 不用 `super::*`：那样会把 gpui 的 `test` 宏一起引进来。
    use super::{Done, Report, describe_skipped, sync_failed};
    use crate::data::NoticeKind;

    fn synced(agent: AgentKind, changed: Option<bool>) -> Synced {
        Synced {
            agent,
            path: PathBuf::from("/tmp/x"),
            result: match changed {
                Some(changed) => Ok(Outcome {
                    changed,
                    backup: None,
                }),
                None => Err(ApplyError::WrongMode { agent }),
            },
        }
    }

    fn texts(report: &Report) -> Vec<(NoticeKind, String)> {
        report
            .notices()
            .into_iter()
            .map(|notice| (notice.kind, notice.text.to_string()))
            .collect()
    }

    fn ok(text: Cow<str>) -> (NoticeKind, String) {
        (NoticeKind::Success, text.into_owned())
    }

    fn err(text: Cow<str>) -> (NoticeKind, String) {
        (NoticeKind::Error, text.into_owned())
    }

    #[test]
    fn a_switch_says_where_and_that_the_catalog_was_not_written() {
        let report = Report::new(Done::Switched {
            agent: AgentKind::Codex,
            to: Some("Relay".into()),
        })
        .catalog(Some(Err(Skipped::Failed)));
        let reason = describe_skipped(Skipped::Failed);
        let switched = t!("write.switched", agent = "Codex", provider = "Relay");
        let skipped = t!("write.codex_catalog_skipped", reason = reason);
        assert_eq!(texts(&report), [ok(switched), err(skipped)]);
    }

    #[test]
    fn a_write_that_failed_says_only_why() {
        let report = Report::new(Done::Switched {
            agent: AgentKind::ClaudeCode,
            to: None,
        })
        .fail("boom");
        assert_eq!(
            texts(&report),
            [err(sync_failed(AgentKind::ClaudeCode, "boom"))]
        );
    }

    #[test]
    fn saving_names_the_agents_written_and_each_one_not_written() {
        let report = Report::new(Done::Saved {
            provider: "Relay".into(),
        })
        .synced(vec![
            synced(AgentKind::ClaudeCode, Some(true)),
            synced(AgentKind::Codex, Some(false)),
            synced(AgentKind::Pi, None),
        ]);
        let error = ApplyError::WrongMode {
            agent: AgentKind::Pi,
        }
        .to_string();
        let saved = t!("toast.saved_synced", agents = "Claude Code");
        let failed = sync_failed(AgentKind::Pi, &error);
        assert_eq!(texts(&report), [ok(saved), err(failed)]);
    }

    #[test]
    fn saving_what_no_agent_uses_names_the_provider() {
        let report = Report::new(Done::Saved {
            provider: "Relay".into(),
        })
        .synced(vec![synced(AgentKind::Codex, Some(false))]);
        let saved = t!("toast.saved", name = "Relay");
        assert_eq!(texts(&report), [ok(saved)]);
    }

    #[test]
    fn the_editor_shows_its_own_failures() {
        let report = Report::new(Done::Deleted {
            provider: "Relay".into(),
        })
        .fail("locked");
        assert!(report.notices().is_empty());
        assert_eq!(report.failed.as_deref(), Some("locked"));
    }

    #[test]
    fn an_update_speaks_only_when_its_own_file_changed() {
        let others = Report::new(Done::Updated {
            agent: AgentKind::ClaudeCode,
        })
        .synced(vec![synced(AgentKind::Codex, Some(true))]);
        assert!(others.notices().is_empty());

        let own = Report::new(Done::Updated {
            agent: AgentKind::ClaudeCode,
        })
        .wrote(
            AgentKind::ClaudeCode,
            &Outcome {
                changed: true,
                backup: None,
            },
        );
        let updated = t!("write.updated", agent = "Claude Code");
        assert_eq!(texts(&own), [ok(updated)]);
    }

    #[test]
    fn adopting_a_key_names_the_provider_when_it_fails() {
        let failed = Report::new(Done::KeyAdopted {
            agent: AgentKind::Codex,
            provider: "Relay".into(),
        })
        .fail("locked");
        let text = t!(
            "write.key_adopt_failed",
            provider = "Relay",
            error = "locked"
        );
        assert_eq!(texts(&failed), [err(text)]);
    }

    #[test]
    fn hiding_an_agent_says_nothing_unless_it_failed() {
        let hidden = Report::new(Done::Hidden {
            agent: AgentKind::Pi,
        });
        assert!(hidden.notices().is_empty());
    }

    #[test]
    fn back_to_the_default_and_ticking_on_and_off() {
        let restored = Report::new(Done::Switched {
            agent: AgentKind::ClaudeCode,
            to: None,
        });
        let text = t!("write.restored", agent = "Claude Code");
        assert_eq!(texts(&restored), [ok(text)]);
        for on in [true, false] {
            let toggled = Report::new(Done::Toggled {
                agent: AgentKind::Pi,
                provider: "Relay".into(),
                on,
            });
            let key = if on {
                "write.enabled"
            } else {
                "write.disabled"
            };
            let text = t!(key, agent = "Pi", provider = "Relay");
            assert_eq!(texts(&toggled), [ok(text)]);
        }
    }

    #[test]
    fn an_adopted_key_names_the_other_agents_written() {
        let alone = Report::new(Done::KeyAdopted {
            agent: AgentKind::Codex,
            provider: "Relay".into(),
        });
        let text = t!("write.key_adopted", provider = "Relay", agent = "Codex");
        assert_eq!(texts(&alone), [ok(text)]);
        let synced = Report::new(Done::KeyAdopted {
            agent: AgentKind::Codex,
            provider: "Relay".into(),
        })
        .synced(vec![synced(AgentKind::ClaudeCode, Some(true))]);
        let text = t!(
            "write.key_adopted_synced",
            provider = "Relay",
            agent = "Codex",
            agents = "Claude Code"
        );
        assert_eq!(texts(&synced), [ok(text)]);
    }

    #[test]
    fn importing_counts_and_restoring_says_the_error_as_is() {
        let one = Report::new(Done::Imported { count: 1 });
        assert_eq!(texts(&one), [ok(t!("transfer.imported_one"))]);
        let three = Report::new(Done::Imported { count: 3 });
        assert_eq!(
            texts(&three),
            [ok(t!("transfer.imported_other", count = 3))]
        );
        let failed = Report::new(Done::Imported { count: 3 }).fail("bad file");
        assert_eq!(
            texts(&failed),
            [err(t!("transfer.import_failed", error = "bad file"))]
        );
        let restored = Report::new(Done::Restored {
            file: "settings.json".into(),
        })
        .fail("conflict");
        assert_eq!(texts(&restored), [err("conflict".into())]);
    }

    #[test]
    fn deleting_names_the_agents_rewritten() {
        let report = Report::new(Done::Deleted {
            provider: "Relay".into(),
        })
        .synced(vec![
            synced(AgentKind::Pi, Some(true)),
            synced(AgentKind::Opencode, Some(true)),
        ]);
        let agents = format!("Pi{}OpenCode", t!("common.list_separator"));
        let text = t!("toast.deleted_synced", agents = agents);
        assert_eq!(texts(&report), [ok(text)]);
    }

    #[test]
    fn a_failed_switch_does_not_mention_the_catalog() {
        let report = Report::new(Done::Switched {
            agent: AgentKind::Codex,
            to: Some("Relay".into()),
        })
        .catalog(Some(Err(Skipped::Failed)))
        .fail("locked");
        assert_eq!(
            texts(&report),
            [err(sync_failed(AgentKind::Codex, "locked"))]
        );
    }
}
