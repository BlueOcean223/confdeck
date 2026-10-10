//! Codex：`~/.codex/config.toml`。单选 agent，列表 + 默认（列表要写进模型目录文件，还没做）。
//!
//! 第三方供应商写成 `model_provider = "custom"` 加 `[model_providers.custom]` 一整张表；
//! 「默认」删掉这两处，回到 Codex 内置的 openai（官方登录）。关键字段的清单和
//! `requires_openai_auth` 的规则移植自 ccs-lite 的 `live/floor.rs`、`live/project/codex.rs`。

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

use super::{AdaptError, AgentKind, choose_protocol, default_model, require_key};
use crate::patch::PatchError;
use crate::patch::toml::{TomlDocPatch, put_value, remove_key, shape_error};
use crate::provider::Provider;

const AGENT: AgentKind = AgentKind::Codex;

/// confdeck 写的路由表的 id。
pub const ROUTE_ID: &str = "custom";

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

/// `config.toml` 顶层的关键字段。另有 `[model_providers.custom]` 整表。
pub const FLOOR_TOP: &[&str] = &[
    // 选路：`openai_base_url` 会把内置 openai 路由改道到别的地址。
    "model_provider",
    "openai_base_url",
    "model",
    "review_model",
    "model_reasoning_effort",
    "plan_mode_reasoning_effort",
    // Codex 已不认这个键，留在这里是为了把旧配置里的残留清掉。
    "disable_response_storage",
    "model_catalog_json",
    // 兜底写法会把 Key 写在顶层。
    "experimental_bearer_token",
    // 旧形态：没有 model_provider 时落在顶层。
    "base_url",
    "wire_api",
];

/// 嵌在用户自己的表里的模型名：只清这几个键，表里的其他键不动。
pub const FLOOR_NESTED: &[&[&str]] = &[
    &["agents", "default_subagent_model"],
    &["agents", "default_subagent_reasoning_effort"],
    &["memories", "extract_model"],
    &["memories", "consolidation_model"],
];

/// 第三方路由表的 `requires_openai_auth`。
///
/// 这个值不决定请求用什么凭据（`experimental_bearer_token` 优先），但决定登录界面：
/// 为 `true` 而 `auth.json` 里没有登录，Codex 会卡在登录页；为 `false` 而旁边留着
/// ChatGPT 登录，Codex 当成已登出（账号信息不显示、token 不刷新）。所以跟着
/// 「盘上有没有登录」走。（ccs-lite 在 Codex 0.149 上核实）
pub fn requires_openai_auth(login_on_disk: bool) -> bool {
    login_on_disk
}

#[derive(Debug, Clone)]
pub struct CodexPatch {
    /// 顶层关键字段的目标值。
    top: Vec<(&'static str, Value)>,
    /// `[model_providers.custom]`；`None` 是「默认」。
    route: Option<Table>,
}

/// 切到这家供应商的补丁；`None` 是「默认」。`login_on_disk`：`~/.codex/auth.json`
/// 里有没有 ChatGPT 登录。
pub fn patch(provider: Option<&Provider>, login_on_disk: bool) -> Result<CodexPatch, AdaptError> {
    let Some(provider) = provider else {
        return Ok(CodexPatch {
            top: Vec::new(),
            route: None,
        });
    };
    let (_, base_url) = choose_protocol(provider, AGENT)?;
    let key = require_key(provider, AGENT)?;
    let model = default_model(provider, AGENT)?;

    let mut table = Table::new();
    table.insert("name", toml_edit::value(provider.name.as_str()));
    table.insert("base_url", toml_edit::value(base_url));
    table.insert("wire_api", toml_edit::value("responses"));
    table.insert("experimental_bearer_token", toml_edit::value(key));
    table.insert(
        "requires_openai_auth",
        toml_edit::value(requires_openai_auth(login_on_disk)),
    );
    let raw = provider.override_for(AGENT).and_then(|o| o.raw.as_ref());
    for (name, value) in raw.into_iter().flatten() {
        if value.is_null() {
            table.remove(name);
            continue;
        }
        let value = to_toml(value).ok_or_else(|| AdaptError::RawNotToml {
            agent: AGENT,
            key: name.clone(),
        })?;
        table.insert(name, Item::Value(value));
    }

    Ok(CodexPatch {
        top: vec![
            ("model_provider", Value::from(ROUTE_ID)),
            ("model", Value::from(model.id.as_str())),
        ],
        route: Some(table),
    })
}

/// JSON → TOML。TOML 没有 null，嵌在数组或对象里的 null 写不了，返回 `None`。
fn to_toml(value: &Json) -> Option<Value> {
    Some(match value {
        Json::Null => return None,
        Json::Bool(flag) => Value::from(*flag),
        Json::Number(number) => match number.as_i64() {
            Some(int) => Value::from(int),
            None => Value::from(number.as_f64()?),
        },
        Json::String(text) => Value::from(text.as_str()),
        Json::Array(items) => Value::Array(items.iter().map(to_toml).collect::<Option<_>>()?),
        Json::Object(map) => {
            let mut table = InlineTable::new();
            for (key, item) in map {
                table.insert(key, to_toml(item)?);
            }
            Value::InlineTable(table)
        }
    })
}

impl TomlDocPatch for CodexPatch {
    fn apply_to(&self, path: &Path, doc: &mut DocumentMut) -> Result<(), PatchError> {
        // 1. 清空顶层关键字段。目标里也有的留给第 4 步原位改值，保住原来的位置和注释。
        let doomed: Vec<String> = doc
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| {
                FLOOR_TOP.contains(&key.as_str())
                    && !self.top.iter().any(|(target, _)| target == key)
            })
            .collect();
        for key in doomed {
            remove_key(doc, &[], &key);
        }

        // 2. 嵌在用户表里的模型名：只清这几个键，表里的其他设置不动。
        for segments in FLOOR_NESTED {
            let (last, parents) = segments.split_last().expect("nested paths");
            remove_key(doc, parents, last);
        }

        // 3. 路由表。
        write_route(path, doc, self.route.as_ref())?;

        // 4. 目标值：已有就原位改，没有就追加。
        for (key, value) in &self.top {
            put_value(doc.as_table_mut(), key, value);
        }
        Ok(())
    }
}

fn write_route(
    path: &Path,
    doc: &mut DocumentMut,
    route: Option<&Table>,
) -> Result<(), PatchError> {
    let root = doc.as_table_mut();
    let container_inline = matches!(root.get("model_providers"), Some(Item::Value(_)));
    let Some(container) = root.get_mut("model_providers") else {
        if let Some(table) = route {
            // 隐式表：只输出 `[model_providers.custom]`，不多一行空的 `[model_providers]`。
            let mut providers = Table::new();
            providers.set_implicit(true);
            providers.insert(ROUTE_ID, Item::Table(table.clone()));
            root.insert("model_providers", Item::Table(providers));
        }
        return Ok(());
    };
    let providers = container
        .as_table_like_mut()
        .ok_or_else(|| shape_error(path, &["model_providers".to_string()]))?;
    match route {
        Some(table) => put_table(providers, ROUTE_ID, table.clone(), container_inline),
        None => {
            let emptied = providers.len() == usize::from(providers.contains_key(ROUTE_ID));
            remove_key(doc, &["model_providers"], ROUTE_ID);
            if emptied {
                remove_key(doc, &[], "model_providers");
            }
        }
    }
    Ok(())
}

/// 按容器原来的形式写表：内联容器（或原来就是内联表）写内联表，否则写标准表，并留在
/// 原来的位置。
fn put_table(providers: &mut dyn TableLike, id: &str, table: Table, container_inline: bool) {
    let existing_inline = matches!(providers.get(id), Some(Item::Value(_)));
    if container_inline || existing_inline {
        let inline = Item::Value(Value::InlineTable(table.into_inline_table()));
        match providers.get_mut(id) {
            Some(slot) => *slot = inline,
            None => {
                providers.insert(id, inline);
            }
        }
        return;
    }
    let mut table = table;
    match providers.get_mut(id) {
        Some(slot) => {
            if let Item::Table(old) = slot {
                *table.decor_mut() = old.decor().clone();
                if let Some(position) = old.position() {
                    table.set_position(position);
                }
            }
            *slot = Item::Table(table);
        }
        None => {
            providers.insert(id, Item::Table(table));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::patch::LivePatch;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    fn apply(patch: &CodexPatch, pre: Option<&str>) -> String {
        let out = patch
            .apply(Path::new("config.toml"), pre.map(str::as_bytes))
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    const LIVE: &str = r#"# 我的 Codex 设置
model = "gpt-5.5"   # 平时用的
model_reasoning_effort = "high"
approval_policy = "on-request"

[agents]
default_subagent_model = "gpt-5.5-mini"
max_threads = 4

[model_providers.custom]
name = "Old"
base_url = "https://old.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-old"

[model_providers.ollama-local]
name = "Ollama"
base_url = "http://localhost:11434/v1"

[tui]
notifications = true
"#;

    #[test]
    fn third_party_replaces_the_route_and_keeps_everything_else() {
        let patch = patch(Some(&relay()), true).unwrap();
        assert_eq!(
            apply(&patch, Some(LIVE)),
            r#"# 我的 Codex 设置
model = "glm-4.6"   # 平时用的
approval_policy = "on-request"
model_provider = "custom"

[agents]
max_threads = 4

[model_providers.custom]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-relay"
requires_openai_auth = true

[model_providers.ollama-local]
name = "Ollama"
base_url = "http://localhost:11434/v1"

[tui]
notifications = true
"#
        );
    }

    #[test]
    fn default_removes_the_route_and_the_floor() {
        let patch = patch(None, false).unwrap();
        assert_eq!(
            apply(&patch, Some(LIVE)),
            r#"# 我的 Codex 设置
approval_policy = "on-request"

[agents]
max_threads = 4

[model_providers.ollama-local]
name = "Ollama"
base_url = "http://localhost:11434/v1"

[tui]
notifications = true
"#
        );
    }

    #[test]
    fn default_drops_an_emptied_model_providers_table() {
        let pre = "model_provider = \"custom\"\n\n[model_providers.custom]\nname = \"x\"\n";
        assert_eq!(apply(&patch(None, false).unwrap(), Some(pre)), "");
    }

    #[test]
    fn a_new_file_gets_an_implicit_container() {
        let patch = patch(Some(&relay()), false).unwrap();
        assert_eq!(
            apply(&patch, None),
            r#"model_provider = "custom"
model = "glm-4.6"

[model_providers.custom]
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-relay"
requires_openai_auth = false
"#
        );
    }

    #[test]
    fn inline_containers_stay_inline() {
        let pre = "model_providers = { custom = { name = \"Old\" } }\n";
        let out = apply(&patch(Some(&relay()), false).unwrap(), Some(pre));
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc["model_providers"].is_inline_table(), "{out}");
        assert_eq!(
            doc["model_providers"]["custom"]["name"].as_str(),
            Some("Relay")
        );
    }

    #[test]
    fn raw_override_merges_into_the_route_table() {
        let mut provider = relay();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                raw: Some(
                    json!({
                        "http_headers": {"X-Team": "infra"},
                        "stream_max_retries": 10,
                        "requires_openai_auth": null
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
                ..AgentOverride::default()
            },
        );
        let out = apply(&patch(Some(&provider), true).unwrap(), None);
        let doc: DocumentMut = out.parse().unwrap();
        let table = &doc["model_providers"]["custom"];
        assert_eq!(table["http_headers"]["X-Team"].as_str(), Some("infra"));
        assert_eq!(table["stream_max_retries"].as_integer(), Some(10));
        assert!(table.get("requires_openai_auth").is_none(), "{out}");

        provider.overrides.get_mut(&AGENT).unwrap().raw =
            Some(json!({"bad": [1, null]}).as_object().unwrap().clone());
        assert!(matches!(
            patch(Some(&provider), true),
            Err(AdaptError::RawNotToml { .. })
        ));
    }

    #[test]
    fn switching_twice_is_byte_stable() {
        let patch = patch(Some(&relay()), true).unwrap();
        let once = apply(&patch, Some(LIVE));
        assert_eq!(apply(&patch, Some(&once)), once);
    }

    #[test]
    fn refuses_a_non_table_container() {
        let err = patch(Some(&relay()), true)
            .unwrap()
            .apply(Path::new("config.toml"), Some(b"model_providers = 1\n"))
            .unwrap_err();
        assert!(matches!(err, PatchError::Shape { .. }), "{err:?}");
    }

    #[test]
    fn refuses_providers_without_a_responses_endpoint_or_key() {
        let mut provider = relay();
        provider
            .endpoints
            .remove(&crate::provider::Protocol::OpenaiResponses);
        assert!(matches!(
            patch(Some(&provider), true),
            Err(AdaptError::Incompatible { .. })
        ));

        let mut provider = relay();
        provider.key.clear();
        assert!(matches!(
            patch(Some(&provider), true),
            Err(AdaptError::MissingKey { .. })
        ));
    }
}
