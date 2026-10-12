//! 一次性的开关（设置页 › Claude Code）：和供应商无关，设一次就不动，所以不走适配器、也不记 `owned`。
//! 三个键在三个文件里，开按键补丁写进去、关把键删掉（fork 的做法），文件其余内容原样保留：
//!
//! | 开关 | 文件 | 键 |
//! | --- | --- | --- |
//! | 跳过首次确认 | `~/.claude.json` | `hasCompletedOnboarding: true` |
//! | VS Code 插件跟着切换 | `~/.claude/config.json` | `primaryApiKey: "any"`，插件看到它就用 settings.json 里的地址和 Key |
//! | 隐藏 AI 署名 | `~/.claude/settings.json` | `includeCoAuthoredBy: false`；关是没有这个键（Claude Code 默认署名） |
//!
//! 三个开关都是「改掉默认行为」：文件不存在、键不存在都算关。关的时候只删 confdeck 会写的那个值：
//! `primaryApiKey` 里是真 Key（老版本 Claude Code 存在这里）就不动。

use std::path::Path;

use serde_json::Value;

use crate::agent::{AgentKind, claude};
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx::Mode;
use crate::patch::json::{self, JsonPatch};
use crate::patch::{Format, KeyPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeToggle {
    /// 跳过首次确认：`~/.claude.json` 的 `hasCompletedOnboarding`。
    SkipOnboarding,
    /// VS Code 插件跟着切换：`~/.claude/config.json` 的 `primaryApiKey`。
    VscodeFollows,
    /// 提交里不带 `Co-Authored-By: Claude`：`~/.claude/settings.json` 的 `includeCoAuthoredBy: false`。
    HideAttribution,
}

/// 设置页里的顺序。
pub const CLAUDE_TOGGLES: &[ClaudeToggle] = &[
    ClaudeToggle::SkipOnboarding,
    ClaudeToggle::VscodeFollows,
    ClaudeToggle::HideAttribution,
];

/// 开关会写的、不在 [`crate::apply::live_files`] 里的文件（备份页要列它们）。
pub fn extra_files(home: &Path) -> Vec<LiveFile> {
    vec![
        ClaudeToggle::SkipOnboarding.file(home),
        ClaudeToggle::VscodeFollows.file(home),
    ]
}

impl ClaudeToggle {
    /// 文件里的键名。
    pub fn key(self) -> &'static str {
        match self {
            Self::SkipOnboarding => "hasCompletedOnboarding",
            Self::VscodeFollows => "primaryApiKey",
            Self::HideAttribution => "includeCoAuthoredBy",
        }
    }

    /// 要写的文件。`settings.json` 和适配器写的是同一个（里面有 Key，0600）；另外两个文件
    /// confdeck 只改这一个键，权限照原样。备份都放在 Claude Code 的架子上。
    pub fn file(self, home: &Path) -> LiveFile {
        match self {
            Self::SkipOnboarding => LiveFile {
                owner: AgentKind::ClaudeCode.id(),
                path: home.join(".claude.json"),
                mode: Mode::Keep,
                format: Format::Json,
            },
            Self::VscodeFollows => LiveFile {
                owner: AgentKind::ClaudeCode.id(),
                path: home.join(".claude").join("config.json"),
                mode: Mode::Keep,
                format: Format::Json,
            },
            Self::HideAttribution => claude::live_file(home),
        }
    }

    /// confdeck 写进文件的值，和它表示的状态；没有这个键（或者是别的值）就是另一个状态。
    fn written(self) -> (Value, bool) {
        match self {
            Self::SkipOnboarding => (Value::Bool(true), true),
            Self::VscodeFollows => (Value::from("any"), true),
            Self::HideAttribution => (Value::Bool(false), true),
        }
    }

    /// 现在是开是关。文件不存在、键不存在算关；文件解析不了报错。
    pub fn read(self, engine: &Engine, home: &Path) -> Result<bool, EngineError> {
        let file = self.file(home);
        let snapshot = engine.read(&file)?;
        let (doc, _) = json::parse(&file.path, snapshot.bytes.as_deref(), false)?;
        let (value, means) = self.written();
        let present = json::value_at(&doc, &KeyPath::new(&[self.key()])) == Some(&value);
        Ok(if present { means } else { !means })
    }

    /// 开或关：往文件里写那个键，或者把它删掉。内容没变时什么都不写（`Outcome::changed` 为假）。
    pub fn set(self, engine: &Engine, home: &Path, on: bool) -> Result<Outcome, EngineError> {
        let (value, means) = self.written();
        let key = KeyPath::new(&[self.key()]);
        let patch = if on == means {
            JsonPatch {
                set: vec![(key, value)],
                ..JsonPatch::default()
            }
        } else {
            JsonPatch {
                remove_if: vec![(key, value)],
                ..JsonPatch::default()
            }
        };
        engine.write(&self.file(home), &patch)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::{Value, json};

    use super::*;
    use crate::apply;
    use crate::data::DataDir;
    use crate::provider::tests::relay;

    struct Home {
        dir: tempfile::TempDir,
        engine: Engine,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let engine = Engine::new(DataDir::new(dir.path().join(".confdeck")));
            Self { dir, engine }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, relative: &str, doc: Value) {
            let path = self.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        }

        fn doc(&self, relative: &str) -> Value {
            serde_json::from_slice(&fs::read(self.path().join(relative)).unwrap()).unwrap()
        }

        fn read(&self, toggle: ClaudeToggle) -> bool {
            toggle.read(&self.engine, self.path()).unwrap()
        }

        fn set(&self, toggle: ClaudeToggle, on: bool) -> bool {
            toggle.set(&self.engine, self.path(), on).unwrap().changed
        }
    }

    #[test]
    fn defaults_when_nothing_is_written() {
        let home = Home::new();
        assert!(!home.read(ClaudeToggle::SkipOnboarding));
        assert!(!home.read(ClaudeToggle::VscodeFollows));
        assert!(!home.read(ClaudeToggle::HideAttribution));
    }

    #[test]
    fn skip_onboarding_writes_and_removes_one_key() {
        let home = Home::new();
        home.write(".claude.json", json!({ "numStartups": 3, "projects": {} }));

        assert!(home.set(ClaudeToggle::SkipOnboarding, true));
        assert_eq!(
            home.doc(".claude.json"),
            json!({ "numStartups": 3, "projects": {}, "hasCompletedOnboarding": true })
        );
        assert!(home.read(ClaudeToggle::SkipOnboarding));
        // 已经开着，再开不写。
        assert!(!home.set(ClaudeToggle::SkipOnboarding, true));

        assert!(home.set(ClaudeToggle::SkipOnboarding, false));
        assert_eq!(
            home.doc(".claude.json"),
            json!({ "numStartups": 3, "projects": {} })
        );
        assert!(!home.read(ClaudeToggle::SkipOnboarding));
    }

    #[test]
    fn missing_file_is_created() {
        let home = Home::new();
        assert!(home.set(ClaudeToggle::SkipOnboarding, true));
        assert_eq!(
            home.doc(".claude.json"),
            json!({ "hasCompletedOnboarding": true })
        );
        assert!(home.set(ClaudeToggle::VscodeFollows, true));
        assert_eq!(
            home.doc(".claude/config.json"),
            json!({ "primaryApiKey": "any" })
        );
    }

    #[test]
    fn vscode_leaves_a_real_key_alone() {
        let home = Home::new();
        home.write(".claude/config.json", json!({ "primaryApiKey": "sk-real" }));
        // 真 Key 不算「跟着切换」，关的时候也不删它。
        assert!(!home.read(ClaudeToggle::VscodeFollows));
        assert!(!home.set(ClaudeToggle::VscodeFollows, false));
        assert_eq!(
            home.doc(".claude/config.json"),
            json!({ "primaryApiKey": "sk-real" })
        );
    }

    #[test]
    fn hiding_attribution_is_a_false_in_settings() {
        let home = Home::new();
        home.write(
            ".claude/settings.json",
            json!({ "theme": "dark", "env": { "ANTHROPIC_MODEL": "x" } }),
        );

        assert!(home.set(ClaudeToggle::HideAttribution, true));
        assert_eq!(
            home.doc(".claude/settings.json"),
            json!({ "theme": "dark", "env": { "ANTHROPIC_MODEL": "x" }, "includeCoAuthoredBy": false })
        );
        assert!(home.read(ClaudeToggle::HideAttribution));

        assert!(home.set(ClaudeToggle::HideAttribution, false));
        assert_eq!(
            home.doc(".claude/settings.json"),
            json!({ "theme": "dark", "env": { "ANTHROPIC_MODEL": "x" } })
        );
        assert!(!home.read(ClaudeToggle::HideAttribution));
    }

    #[test]
    fn explicit_true_counts_as_shown() {
        let home = Home::new();
        home.write(
            ".claude/settings.json",
            json!({ "includeCoAuthoredBy": true }),
        );
        assert!(!home.read(ClaudeToggle::HideAttribution));
        // 用户自己写的 true 留着。
        assert!(!home.set(ClaudeToggle::HideAttribution, false));
        assert_eq!(
            home.doc(".claude/settings.json"),
            json!({ "includeCoAuthoredBy": true })
        );
    }

    #[test]
    fn switching_provider_keeps_the_attribution_key() {
        let home = Home::new();
        home.write(".claude/settings.json", json!({}));
        home.set(ClaudeToggle::HideAttribution, true);

        let relay = relay();
        apply::switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay),
        )
        .unwrap();
        assert!(home.read(ClaudeToggle::HideAttribution));
        apply::switch(&home.engine, home.path(), AgentKind::ClaudeCode, None).unwrap();
        assert!(home.read(ClaudeToggle::HideAttribution));
    }

    #[test]
    fn unparsable_file_is_an_error_not_a_default() {
        let home = Home::new();
        fs::write(home.path().join(".claude.json"), "{ nope").unwrap();
        assert!(
            ClaudeToggle::SkipOnboarding
                .read(&home.engine, home.path())
                .is_err()
        );
    }
}
