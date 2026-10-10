//! Codex：`~/.codex/config.toml`。单选 agent，列表 + 默认（列表要写进模型目录文件，还没做）。
//!
//! 第三方供应商写成 `model_provider = "custom"` 加 `[model_providers.custom]` 一整张表；
//! 「默认」删掉这两处，回到 Codex 内置的 openai（官方登录）。关键字段的清单和
//! `requires_openai_auth` 的规则移植自 ccs-lite 的 `live/floor.rs`、`live/project/codex.rs`。

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

use super::{AdaptError, AgentKind, Connection, choose_protocol, default_model, require_key};
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::PatchError;
use crate::patch::toml::{self, TomlDocPatch, put_value, remove_key, shape_error};
use crate::provider::{Protocol, Provider, normalize_url};

const AGENT: AgentKind = AgentKind::Codex;

/// confdeck 写的路由表的 id。
pub const ROUTE_ID: &str = "custom";

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

/// 里面有第三方的 Key，按 0600 写。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: config_path(home),
        mode: Mode::Private,
    }
}

/// Codex 存 ChatGPT 登录的文件。
pub fn auth_path(home: &Path) -> PathBuf {
    home.join(".codex").join("auth.json")
}

/// `auth.json` 里有没有 Codex 能直接用的登录（OAuth token 等），只有 `OPENAI_API_KEY` 不算。
/// 判断规则移植自 ccs-lite 的 `codex_auth_has_credential_login_material`。
///
/// `config.toml` 里 `cli_auth_credentials_store` 设成 keyring / auto 时登录存在系统钥匙串里，
/// 这里看不到，当成没有登录。
pub fn login_on_disk(auth: Option<&[u8]>) -> bool {
    let Some(auth) = auth.and_then(|bytes| serde_json::from_slice::<Json>(bytes).ok()) else {
        return false;
    };
    let present = |value: &Json| match value {
        Json::Null => false,
        Json::String(text) => !text.trim().is_empty(),
        Json::Object(map) => !map.is_empty(),
        Json::Array(items) => !items.is_empty(),
        _ => true,
    };
    let carrier = ["personal_access_token", "agent_identity", "bedrock_api_key"]
        .iter()
        .any(|key| auth.get(key).is_some_and(present));
    let tokens = auth
        .get("tokens")
        .and_then(Json::as_object)
        .is_some_and(|tokens| {
            ["id_token", "access_token", "refresh_token"]
                .iter()
                .any(|key| tokens.get(*key).is_some_and(present))
        });
    carrier || tokens
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

/// 从 `config.toml` 读出现在连的是哪。只看选的路由和它的地址、凭据，不看模型名。
pub fn connection(path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
    let doc = toml::parse(path, bytes)?;
    let text = |item: Option<&Item>| {
        item.and_then(Item::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let selected = text(doc.get("model_provider")).unwrap_or_else(|| "openai".to_string());
    if selected == "openai" {
        // `openai_base_url` 把内置的 openai 路由改道到别的地址。
        return Ok(match text(doc.get("openai_base_url")) {
            Some(url) => Connection::Endpoint {
                base_url: normalize_url(Protocol::OpenaiResponses, &url),
                credential: None,
            },
            None => Connection::Default,
        });
    }
    let table = doc
        .get("model_providers")
        .and_then(|providers| providers.get(&selected))
        .and_then(Item::as_table_like);
    let Some(table) = table else {
        // 没有表的是 Codex 内置的 ollama、lmstudio 等。
        return Ok(Connection::Other { host: None });
    };
    let Some(base_url) = text(table.get("base_url")) else {
        return Ok(Connection::Other { host: None });
    };
    let credential = text(table.get("experimental_bearer_token"))
        .or_else(|| text(table.get("env_key")).map(|name| format!("env: {name}")));
    Ok(Connection::Endpoint {
        base_url: normalize_url(Protocol::OpenaiResponses, &base_url),
        credential,
    })
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
    fn login_on_disk_needs_real_credentials() {
        assert!(!login_on_disk(None));
        assert!(!login_on_disk(Some(b"{ broken")));
        assert!(!login_on_disk(Some(
            br#"{"OPENAI_API_KEY": "sk-x", "last_refresh": "2026-10-01"}"#
        )));
        assert!(!login_on_disk(Some(
            br#"{"tokens": {"account_id": "acct", "id_token": ""}}"#
        )));
        assert!(login_on_disk(Some(
            br#"{"tokens": {"refresh_token": "rt"}}"#
        )));
        assert!(login_on_disk(Some(br#"{"agent_identity": {"id": "a"}}"#)));
    }

    fn read(text: &str) -> Connection {
        connection(Path::new("config.toml"), Some(text.as_bytes())).unwrap()
    }

    #[test]
    fn connection_follows_the_selected_route() {
        assert_eq!(
            connection(Path::new("c.toml"), None).unwrap(),
            Connection::Default
        );
        assert_eq!(read("model = \"gpt-5.5\"\n"), Connection::Default);
        assert_eq!(read("model_provider = \"openai\"\n"), Connection::Default);
        assert_eq!(
            read("openai_base_url = \"https://proxy.example/v1/\"\n"),
            Connection::Endpoint {
                base_url: "https://proxy.example/v1".into(),
                credential: None
            }
        );
        assert_eq!(
            read(LIVE),
            Connection::Default,
            "LIVE has no model_provider"
        );
        assert_eq!(
            read(
                "model_provider = \"ollama-local\"\n[model_providers.ollama-local]\nbase_url = \"http://localhost:11434/v1\"\nenv_key = \"OLLAMA_KEY\"\n"
            ),
            Connection::Endpoint {
                base_url: "http://localhost:11434/v1".into(),
                credential: Some("env: OLLAMA_KEY".into())
            }
        );
        assert_eq!(
            read("model_provider = \"ollama\"\n"),
            Connection::Other { host: None }
        );
    }

    #[test]
    fn what_patch_writes_reads_back_as_the_same_connection() {
        let out = patch(Some(&relay()), true)
            .unwrap()
            .apply(Path::new("c.toml"), Some(LIVE.as_bytes()))
            .unwrap();
        assert_eq!(
            connection(Path::new("c.toml"), Some(&out)).unwrap(),
            Connection::Endpoint {
                base_url: "https://relay.example/v1".into(),
                credential: Some("sk-relay".into())
            }
        );
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
