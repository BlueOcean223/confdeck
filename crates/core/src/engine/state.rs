//! `~/.confdeck/state.json`：只属于这台设备的记录，不同步。
//!
//! - `files`：每个 agent 配置文件上次由 confdeck 写入后的 hash。写之前拿文件的当前内容和这里比：
//!   一样说明文件还是 confdeck 上次写的样子，不用备份；不一样说明被外部改过。
//! - `selections`：单选 agent 上次在 confdeck 里选的是哪家（`null` 是「默认」，没有这一项是没选过）。
//!   认当前是哪家时，同一个地址和 Key 对得上好几家，就以它为准（见 [`crate::current`]）。
//! - `owned`：原始片段写进 agent 配置里和用户共用的位置的键和值（现在只有 Claude Code 的 `env` 和顶层）。
//!   下次切换时，值还是这个的才删；用户改过的留着。记在这里而不是现查上一家供应商：那家可能已经改了或删了。
//! - `hidden_on_page`、`hidden_in_panel`：用户在设置页「显示哪些 agent」里关掉的 agent（id），
//!   分别对应主窗口的 Agent 页和菜单栏面板。没写就是都显示，新接上的 agent 自然也显示。
//! - `proxy`：设置页的出站代理（[`crate::net::Proxy`]）。没写是「跟随系统」。代理多半是本机的端口，所以只存本机。
//! - `checks`：每家供应商、每个官方账号的额度查询设置（[`crate::quota::Check`]），键是 `provider:<id>`、`account:<agent>:<id>`。
//!   没写是默认值。多久查一次是这台设备的事，不同步。自己配的查法（[`crate::query::Query`]）也在这里，里面可能有站点的访问令牌，
//!   所以不随供应商导出（文件只有自己能读）。
//!
//! 这份记录只是提示，丢了或读不出来就当作空的：后果只是多备份一次、认不出同地址的几家里是哪一家，
//! 所以不像用户的文件那样「读不出就报错」。

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentKind;
use crate::fsx::{self, Digest, Mode};
use crate::net::Proxy;
use crate::patch::KeyPath;
use crate::quota::{Check, CheckKey};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    version: u32,
    /// 键是解析过软链接的绝对路径。
    #[serde(default)]
    files: BTreeMap<String, Record>,
    /// 键是 agent 的 id。
    #[serde(default)]
    selections: BTreeMap<String, Option<String>>,
    /// 键是 agent 的 id。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    owned: BTreeMap<String, Vec<Owned>>,
    /// Agent 页上藏起来的 agent（id）。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    hidden_on_page: BTreeSet<String>,
    /// 菜单栏面板里藏起来的 agent（id）。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    hidden_in_panel: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Proxy::is_default")]
    proxy: Proxy,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    checks: BTreeMap<String, Check>,
}

/// `state.json` 里本机的设置：藏起来的 agent、出站代理、额度查询、单选 agent 上次选的哪家。
/// 读一次用到底（[`super::Engine::local`]），不每用一样读一次文件。
#[derive(Debug, Clone, Default)]
pub struct Local(pub(super) State);

impl Local {
    /// 这一处藏起来的 agent（id）。
    pub fn hidden(&self, place: Place) -> &BTreeSet<String> {
        self.0.hidden(place)
    }

    pub fn proxy(&self) -> &Proxy {
        self.0.proxy()
    }

    /// 一家供应商、一个账号的额度查询设置；没设过是默认值。
    pub fn check(&self, key: &CheckKey) -> Check {
        self.0.check(key)
    }

    /// 单选 agent 上次在 confdeck 里选的是哪家：`None` 是没选过，`Some(None)` 是「默认」。
    pub fn selection(&self, agent: AgentKind) -> Option<Option<&str>> {
        self.0.selection(agent.id())
    }
}

/// 「显示哪些 agent」分两处：主窗口的 Agent 页、菜单栏面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Page,
    Panel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Owned {
    pub path: KeyPath,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub sha256: String,
    /// 本地时间，`2026-10-10T15:00:02`。
    pub written_at: String,
}

impl State {
    pub fn load(path: &Path) -> Self {
        fsx::read_optional(path)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        self.version = VERSION;
        let mut bytes = serde_json::to_vec_pretty(self).expect("state always serializes");
        bytes.push(b'\n');
        fsx::write_atomic(path, &bytes, Mode::Private)
    }

    pub fn get(&self, target: &Path) -> Option<&Record> {
        self.files.get(&key(target))
    }

    /// 文件的当前内容是不是 confdeck 上次写的。
    pub fn is_ours(&self, target: &Path, current: Option<&Digest>) -> bool {
        match (self.get(target), current) {
            (Some(record), Some(current)) => record.sha256 == current.0,
            _ => false,
        }
    }

    /// 上次选的：`None` 是没选过，`Some(None)` 是「默认」。
    pub fn selection(&self, agent: &str) -> Option<Option<&str>> {
        self.selections.get(agent).map(Option::as_deref)
    }

    pub fn select(&mut self, agent: &str, provider_id: Option<&str>) {
        self.selections
            .insert(agent.to_string(), provider_id.map(str::to_string));
    }

    /// 这一处藏起来的 agent。
    pub fn hidden(&self, place: Place) -> &BTreeSet<String> {
        match place {
            Place::Page => &self.hidden_on_page,
            Place::Panel => &self.hidden_in_panel,
        }
    }

    pub fn set_hidden(&mut self, place: Place, agent: &str, hidden: bool) {
        let set = match place {
            Place::Page => &mut self.hidden_on_page,
            Place::Panel => &mut self.hidden_in_panel,
        };
        if hidden {
            set.insert(agent.to_string());
        } else {
            set.remove(agent);
        }
    }

    pub fn proxy(&self) -> &Proxy {
        &self.proxy
    }

    pub fn set_proxy(&mut self, proxy: Proxy) {
        self.proxy = proxy;
    }

    /// 没设过是默认值。
    pub fn check(&self, key: &CheckKey) -> Check {
        self.checks.get(&key.stored()).cloned().unwrap_or_default()
    }

    /// 默认值不存。
    pub fn set_check(&mut self, key: &CheckKey, check: Check) {
        if check.is_default() {
            self.checks.remove(&key.stored());
        } else {
            self.checks.insert(key.stored(), check);
        }
    }

    pub fn owned(&self, agent: &str) -> &[Owned] {
        self.owned.get(agent).map_or(&[], Vec::as_slice)
    }

    pub fn own(&mut self, agent: &str, entries: Vec<Owned>) {
        if entries.is_empty() {
            self.owned.remove(agent);
        } else {
            self.owned.insert(agent.to_string(), entries);
        }
    }

    pub fn record(&mut self, target: &Path, written: &Digest, at: &str) {
        self.files.insert(
            key(target),
            Record {
                sha256: written.0.clone(),
                written_at: at.to_string(),
            },
        );
    }
}

fn key(target: &Path) -> String {
    target.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn checks_keep_only_what_was_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::load(&path);
        let deepseek = CheckKey::Provider("deepseek".into());
        let codex = CheckKey::Account(crate::agent::AgentKind::Codex, "abc".into());
        let grok = CheckKey::Login(crate::agent::AgentKind::GrokBuild);
        state.set_check(
            &grok,
            Check {
                off: true,
                ..Default::default()
            },
        );
        state.set_check(
            &deepseek,
            Check {
                off: true,
                ..Default::default()
            },
        );
        state.set_check(
            &codex,
            Check {
                every: Some(15),
                ..Default::default()
            },
        );
        state.set_check(&CheckKey::Provider("kimi".into()), Check::default());
        state.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("kimi"),
            "default values are not stored: {text}"
        );
        let state = State::load(&path);
        assert!(text.contains("\"provider:deepseek\"") && text.contains("\"account:codex:abc\""));
        assert!(text.contains("\"login:grok-build\""), "{text}");
        assert!(state.check(&grok).off);
        assert!(state.check(&deepseek).off);
        assert_eq!(state.check(&codex).every, Some(15));
        // 在用的默认 5 分钟，不在用的不自动查；关掉的都不查；设了的照设的。
        assert_eq!(Check::default().every(true), Check::DEFAULT_EVERY);
        assert_eq!(Check::default().every(false), 0);
        assert_eq!(state.check(&deepseek).every(true), 0);
        assert_eq!(state.check(&codex).every(false), 15);
    }

    #[test]
    fn round_trips_and_tolerates_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let target = Path::new("/home/u/.claude/settings.json");
        let digest = Digest::of(b"x");

        let mut state = State::load(&path);
        assert!(!state.is_ours(target, Some(&digest)));
        state.record(target, &digest, "2026-10-10T15:00:02");
        state.save(&path).unwrap();

        let state = State::load(&path);
        assert!(state.is_ours(target, Some(&digest)));
        assert!(!state.is_ours(target, Some(&Digest::of(b"y"))));
        assert!(!state.is_ours(target, None));

        let mut state = State::load(&path);
        assert_eq!(state.selection("codex"), None);
        state.select("codex", None);
        state.select("claude-code", Some("relay"));
        state.save(&path).unwrap();
        let state = State::load(&path);
        assert_eq!(state.selection("codex"), Some(None));
        assert_eq!(state.selection("claude-code"), Some(Some("relay")));
        assert!(state.is_ours(target, Some(&digest)), "files are kept");

        let mut state = State::load(&path);
        let owned = Owned {
            path: KeyPath::new(&["env", "API_TIMEOUT_MS"]),
            value: Value::from("600000"),
        };
        state.own("claude-code", vec![owned.clone()]);
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).owned("claude-code"), [owned]);

        let mut state = State::load(&path);
        assert!(state.hidden(Place::Panel).is_empty());
        state.set_hidden(Place::Panel, "codex", true);
        state.set_hidden(Place::Page, "grok-build", true);
        state.set_hidden(Place::Page, "grok-build", false);
        state.save(&path).unwrap();
        let state = State::load(&path);
        assert_eq!(
            state.hidden(Place::Panel).iter().collect::<Vec<_>>(),
            ["codex"]
        );
        assert!(state.hidden(Place::Page).is_empty());
        assert_eq!(
            state.selection("claude-code"),
            Some(Some("relay")),
            "other records are kept"
        );
        let mut state = State::load(&path);
        state.own("claude-code", Vec::new());
        state.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("owned"));

        fs::write(&path, "{ broken").unwrap();
        assert!(State::load(&path).get(target).is_none());
    }
}
