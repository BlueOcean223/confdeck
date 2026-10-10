//! Grok Build：`~/.grok/config.toml`。单选 agent，列表 + 默认。
//!
//! 每个模型一张表 `[model."confdeck-<模型 id>"]`（`model`、`name`、`base_url`、`api_key`、
//! `api_backend`、`context_window`），`[models] default` 指向默认模型那一张。在 Grok 里按 Ctrl+M
//! 可以在这家的几个模型之间换。
//!
//! confdeck 写的表靠 `confdeck-` 前缀认：切换时全部删掉再写目标的，用户自己写的 `[model.*]` 表不动。
//! 不靠写入记录：Grok 的 `/settings` 会把 `models.default` 改成内置模型，按它找表会漏删；
//! 记录丢了也会漏删。
//!
//! 字段和凭据的规则见 Grok 自带文档 `~/.grok/docs/user-guide/11-custom-models.md`（Grok Build 1.0.46 核实）。
//!
//! Anthropic 协议（`api_backend = "messages"`）：请求发到 `{base_url}/messages`，所以地址要带 `/v1`；
//! Key 按 `Authorization: Bearer` 发，不发 `x-api-key` 和 `anthropic-version`（10-10 在 1.0.46 上抓包核实，
//! 和 xai-org/grok-build 源码里 `AuthScheme` 默认 `Bearer` 一致）。第三方 Anthropic 兼容地址都收 Bearer
//! （Claude Code 的 `ANTHROPIC_AUTH_TOKEN` 也是这么发的）；Anthropic 官方要 `x-api-key`，不支持。

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, TableLike};

use super::{
    AdaptError, AgentKind, Connection, Credential, FileSetup, choose_protocol, default_model,
    list_models, require_key,
};
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::toml::{
    self, TomlDocPatch, from_json, put_table, put_value, remove_key, shape_error,
};
use crate::patch::{Format, PatchError};
use crate::provider::{Model, Protocol, Provider, normalize_url};

const AGENT: AgentKind = AgentKind::GrokBuild;

/// confdeck 写的模型表的表名前缀，用来和用户自己写的表区分。
pub const TABLE_PREFIX: &str = "confdeck-";

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".grok").join("config.toml")
}

/// 里面有 Key，按 0600 写。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: config_path(home),
        mode: Mode::Private,
        format: Format::Toml,
    }
}

/// 这个模型的表名，也是在 Grok 里 `/model`、`-m` 用的名字。
pub fn table_name(model_id: &str) -> String {
    format!("{TABLE_PREFIX}{model_id}")
}

/// 除了 `models.default`，这些键也按表名引用模型。引用的是 confdeck 删掉的表时一并删掉，
/// 不然 Grok 找不到这个模型。
const MODEL_REFS: &[(&str, &str)] = &[
    ("models", "web_search"),
    ("models", "session_summary"),
    ("models", "image_description"),
    ("models", "prompt_suggestion"),
    ("ui", "fork_secondary_model"),
];

/// Grok 的 `api_backend`。没写时 Grok 用 `chat_completions`。
fn api_backend(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiResponses => "responses",
        Protocol::OpenaiChat => "chat_completions",
        Protocol::Anthropic => "messages",
        Protocol::Gemini => unreachable!("Grok Build 不支持 Gemini 协议，choose_protocol 不会选到"),
    }
}

fn protocol_of(backend: Option<&str>) -> Option<Protocol> {
    match backend.unwrap_or("chat_completions") {
        "responses" => Some(Protocol::OpenaiResponses),
        "chat_completions" => Some(Protocol::OpenaiChat),
        "messages" => Some(Protocol::Anthropic),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct GrokPatch {
    /// 目标的模型表（表名，表），按模型列表的顺序；空是「默认」。
    tables: Vec<(String, Table)>,
    /// `models.default`；`None` 是「默认」。
    default: Option<String>,
}

/// 切到这家供应商的补丁；`None` 是「默认」：删掉 confdeck 的表、`[endpoints] models_base_url`，
/// `models.default` 指向别的地址时也删掉，回到 Grok 内置的模型和自带的登录。默认的是内置模型
/// （用户在 Grok 里选的）就留着。
///
/// 第三方必须有 Key：模型表里没有 Key 时，Grok 会拿 `grok login` 的凭据或 `XAI_API_KEY` 去请求这个地址。
pub fn patch(provider: Option<&Provider>) -> Result<GrokPatch, AdaptError> {
    let Some(provider) = provider else {
        return Ok(GrokPatch {
            tables: Vec::new(),
            default: None,
        });
    };
    let (protocol, base_url) = choose_protocol(provider, AGENT)?;
    let base_url = match protocol {
        Protocol::Anthropic if provider.is_anthropic_official() => {
            return Err(AdaptError::AnthropicOfficial { agent: AGENT });
        }
        // confdeck 存的 Anthropic 地址不带 `/v1`，Grok 只在后面拼 `/messages`。
        Protocol::Anthropic => format!("{base_url}/v1"),
        _ => base_url.to_string(),
    };
    let key = require_key(provider, AGENT)?;
    let default = default_model(provider, AGENT)?;
    let mut models = list_models(provider, AGENT)?;
    // 覆盖里指定的默认模型可以不在列表里，它的表也要写。
    if !models.iter().any(|model| model.id == default.id) {
        models.push(default);
    }
    let raw = provider.override_for(AGENT).and_then(|o| o.raw.as_ref());

    let mut tables = Vec::new();
    for model in models {
        let mut table = model_table(provider, model, protocol, &base_url, key);
        for (name, value) in raw.into_iter().flatten() {
            if value.is_null() {
                table.remove(name);
                continue;
            }
            let value = from_json(value).ok_or_else(|| AdaptError::RawNotToml {
                agent: AGENT,
                key: name.clone(),
            })?;
            table.insert(name, Item::Value(value));
        }
        tables.push((table_name(&model.id), table));
    }
    Ok(GrokPatch {
        tables,
        default: Some(table_name(&default.id)),
    })
}

fn model_table(
    provider: &Provider,
    model: &Model,
    protocol: Protocol,
    base_url: &str,
    key: &str,
) -> Table {
    let label = model.name.as_deref().unwrap_or(&model.id);
    let mut table = Table::new();
    table.insert("model", toml_edit::value(model.id.as_str()));
    table.insert(
        "name",
        toml_edit::value(format!("{label} ({})", provider.name)),
    );
    table.insert("base_url", toml_edit::value(base_url));
    table.insert("api_key", toml_edit::value(key));
    table.insert("api_backend", toml_edit::value(api_backend(protocol)));
    // 没写时 Grok 对新模型按 200,000 算，到时候自动压缩。
    if let Some(context) = model
        .context
        .and_then(|context| i64::try_from(context).ok())
    {
        table.insert("context_window", toml_edit::value(context));
    }
    table
}

impl TomlDocPatch for GrokPatch {
    fn apply_to(&self, path: &Path, doc: &mut DocumentMut) -> Result<(), PatchError> {
        let had_content = |doc: &DocumentMut, key: &str| {
            doc.get(key)
                .and_then(Item::as_table_like)
                .is_some_and(|table| !table.is_empty())
        };
        let had = [
            had_content(doc, "models"),
            had_content(doc, "model"),
            had_content(doc, "endpoints"),
        ];
        let targets: Vec<&str> = self.tables.iter().map(|(name, _)| name.as_str()).collect();

        // 1. `models.default`。新文件里 `[models]` 排在模型表前面。
        match &self.default {
            Some(name) => {
                let models = ensure_table(path, doc, "models", false)?;
                put_value(models, "default", &toml_edit::Value::from(name.as_str()));
            }
            None => {
                if custom_default(doc).is_some() {
                    remove_key(doc, &["models"], "default");
                }
                remove_key(doc, &["endpoints"], "models_base_url");
            }
        }

        // 2. confdeck 的旧表删掉，目标的表原位替换或追加。
        let ours: Vec<String> = doc
            .get("model")
            .map(|item| {
                item.as_table_like()
                    .ok_or_else(|| shape_error(path, &["model".to_string()]))
            })
            .transpose()?
            .map(|tables| {
                tables
                    .iter()
                    .map(|(name, _)| name)
                    .filter(|name| name.starts_with(TABLE_PREFIX) && !targets.contains(name))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for name in ours {
            remove_key(doc, &["model"], &name);
        }
        if !self.tables.is_empty() {
            let inline = matches!(doc.get("model"), Some(Item::Value(_)));
            let container = ensure_table(path, doc, "model", true)?;
            for (name, table) in &self.tables {
                put_table(container, name, table.clone(), inline);
            }
        }

        // 3. 其他引用了 confdeck 删掉的表的键。
        for (parent, key) in MODEL_REFS {
            let dangling = doc
                .get(parent)
                .and_then(|table| table.get(key))
                .and_then(Item::as_str)
                .is_some_and(|name| name.starts_with(TABLE_PREFIX) && !targets.contains(&name));
            if dangling {
                remove_key(doc, &[*parent], key);
            }
        }

        // 4. 被上面几步删空的表一并删掉，原来就是空的不动。
        for (key, had) in ["models", "model", "endpoints"].into_iter().zip(had) {
            let emptied = doc
                .get(key)
                .and_then(Item::as_table_like)
                .is_some_and(TableLike::is_empty);
            if had && emptied {
                remove_key(doc, &[], key);
            }
        }
        Ok(())
    }
}

/// 根下的表，缺的时候建一张（`implicit`：只装子表时不输出空表头）。
fn ensure_table<'a>(
    path: &Path,
    doc: &'a mut DocumentMut,
    key: &str,
    implicit: bool,
) -> Result<&'a mut dyn TableLike, PatchError> {
    let root = doc.as_table_mut();
    if !root.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(implicit);
        root.insert(key, Item::Table(table));
    }
    root.get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| shape_error(path, &[key.to_string()]))
}

/// 从 `config.toml` 读出现在连的是哪：`models.default` 那张模型表的地址和凭据，不看模型名。
///
/// 没有默认模型、或默认的是内置模型是「默认」，除非 `[endpoints] models_base_url` 把内置模型
/// 改道到了别的地址。内置模型可以有自己的表，只改温度等参数；表里有凭据、没有地址的
/// （给内置模型换了个 Key）算「其他接法」。
pub fn connection(path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
    let doc = toml::parse(path, bytes)?;
    Ok(match custom_default(&doc) {
        Some((_, table)) => match endpoint(&doc, table) {
            Some((protocol, base_url, credential)) => Connection::Endpoint {
                base_url: normalize_url(protocol, &base_url),
                credential,
            },
            None => Connection::Other { host: None },
        },
        None => match rerouted(&doc) {
            Some(base_url) => Connection::Endpoint {
                base_url: normalize_url(Protocol::OpenaiChat, &base_url),
                credential: None,
            },
            None => Connection::Default,
        },
    })
}

/// 「把当前配置存成供应商」：默认模型那张表的地址、Key，和连同一个地址的几张表里的模型。
/// 连的不是一个地址时为 `None`。
pub fn setup(path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
    let doc = toml::parse(path, bytes)?;
    let Some((name, table)) = custom_default(&doc) else {
        return Ok(None);
    };
    let Some((protocol, base_url, credential)) = endpoint(&doc, table) else {
        return Ok(None);
    };
    let base_url = normalize_url(protocol, &base_url);
    let default = model_id(name, table);
    let mut models = vec![default.clone()];
    let tables = doc.get("model").and_then(Item::as_table_like);
    for (other, item) in tables.into_iter().flat_map(|tables| tables.iter()) {
        let Some(other_table) = item.as_table_like() else {
            continue;
        };
        let same_address = endpoint(&doc, other_table)
            .is_some_and(|(p, url, _)| p == protocol && normalize_url(p, &url) == base_url);
        let id = model_id(other, other_table);
        if same_address && !models.contains(&id) {
            models.push(id);
        }
    }
    Ok(Some(FileSetup {
        protocol,
        base_url,
        key: credential
            .as_ref()
            .and_then(Credential::key)
            .map(str::to_string),
        models,
        slots: Default::default(),
        default_model: Some(default),
    }))
}

fn text(item: Option<&Item>) -> Option<String> {
    item.and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `models.default` 和它指向的模型表。
fn default_table(doc: &DocumentMut) -> Option<(&str, &dyn TableLike)> {
    let name = doc.get("models")?.get("default")?.as_str()?.trim();
    let (name, item) = doc.get("model")?.as_table_like()?.get_key_value(name)?;
    Some((name.get(), item.as_table_like()?))
}

/// `models.default` 指向的表有自己的地址或凭据时，返回它：用的不是内置模型和 Grok 自带的登录。
fn custom_default(doc: &DocumentMut) -> Option<(&str, &dyn TableLike)> {
    default_table(doc).filter(|(_, table)| {
        endpoint(doc, *table).is_some() || credential_in(doc, *table).is_some()
    })
}

/// `[endpoints] models_base_url`：内置模型改道到的地址。
fn rerouted(doc: &DocumentMut) -> Option<String> {
    text(doc.get("endpoints").and_then(|e| e.get("models_base_url")))
}

/// 模型表请求哪个地址、用什么协议和凭据。表里没写的从 `model_provider` 指的
/// `[model_providers.<id>]` 继承。没有地址时为 `None`。
fn endpoint(
    doc: &DocumentMut,
    table: &dyn TableLike,
) -> Option<(Protocol, String, Option<Credential>)> {
    let shared = text(table.get("model_provider")).and_then(|id| {
        doc.get("model_providers")?
            .get(&id)?
            .as_table_like()
            .map(|shared| shared as &dyn TableLike)
    });
    let field = |key: &str| {
        text(table.get(key)).or_else(|| shared.and_then(|shared| text(shared.get(key))))
    };
    let base_url = field("base_url")?;
    let protocol = protocol_of(field("api_backend").as_deref())?;
    let credential = [Some(table), shared]
        .into_iter()
        .flatten()
        .find_map(|source| credential_in(doc, source));
    Some((protocol, base_url, credential))
}

/// Grok 按这个顺序找 Key：`api_key`、`env_key`（一个名字或数组）、`auth_provider` 指的命令。
fn credential_in(doc: &DocumentMut, table: &dyn TableLike) -> Option<Credential> {
    if let Some(key) = text(table.get("api_key")) {
        return Some(Credential::Key(key));
    }
    let env = match table.get("env_key") {
        Some(Item::Value(toml_edit::Value::Array(names))) => names
            .iter()
            .filter_map(|name| name.as_str())
            .map(str::trim)
            .find(|name| !name.is_empty())
            .map(str::to_string),
        other => text(other),
    };
    if let Some(name) = env {
        return Some(Credential::EnvVar(name));
    }
    let helper = text(table.get("auth_provider"))?;
    let command = doc
        .get("auth_provider")
        .and_then(|providers| providers.get(&helper))
        .and_then(|provider| text(provider.get("command")));
    Some(Credential::Helper(command.unwrap_or(helper)))
}

/// 表里的 `model`；没写时表名就是模型 id（覆盖内置模型的写法）。
fn model_id(name: &str, table: &dyn TableLike) -> String {
    text(table.get("model")).unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::patch::LivePatch;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    fn apply(patch: &GrokPatch, pre: Option<&str>) -> String {
        let out = patch
            .apply(Path::new("config.toml"), pre.map(str::as_bytes))
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn read(text: &str) -> Connection {
        connection(Path::new("config.toml"), Some(text.as_bytes())).unwrap()
    }

    #[test]
    fn a_new_file_gets_one_table_per_model_and_the_default() {
        assert_eq!(
            apply(&patch(Some(&relay())).unwrap(), None),
            r#"[models]
default = "confdeck-glm-4.6"

[model."confdeck-glm-4.5-air"]
model = "glm-4.5-air"
name = "glm-4.5-air (Relay)"
base_url = "https://relay.example/v1"
api_key = "sk-relay"
api_backend = "responses"

[model."confdeck-glm-4.6"]
model = "glm-4.6"
name = "GLM 4.6 (Relay)"
base_url = "https://relay.example/v1"
api_key = "sk-relay"
api_backend = "responses"
context_window = 200000
"#
        );
    }

    const LIVE: &str = r#"# 我的 Grok
[ui]
fork_secondary_model = "confdeck-old"

[models]
# 新会话用的
default = "confdeck-old"
default_reasoning_effort = "high"
web_search = "grok-4.5"

[model.confdeck-old]
model = "old"
base_url = "https://old.example/v1"
api_key = "sk-old"

[model.mine]
model = "llama"
base_url = "http://localhost:11434/v1"

[mcp_servers.fs]
command = "fs"
"#;

    #[test]
    fn switching_replaces_confdeck_tables_and_keeps_the_users() {
        let mut provider = relay();
        provider.models.retain(|model| model.id == "glm-4.6");
        assert_eq!(
            apply(&patch(Some(&provider)).unwrap(), Some(LIVE)),
            r#"# 我的 Grok
[ui]

[models]
# 新会话用的
default = "confdeck-glm-4.6"
default_reasoning_effort = "high"
web_search = "grok-4.5"

[model.mine]
model = "llama"
base_url = "http://localhost:11434/v1"

[model."confdeck-glm-4.6"]
model = "glm-4.6"
name = "GLM 4.6 (Relay)"
base_url = "https://relay.example/v1"
api_key = "sk-relay"
api_backend = "responses"
context_window = 200000

[mcp_servers.fs]
command = "fs"
"#
        );
    }

    #[test]
    fn default_removes_confdeck_tables_and_a_custom_default_only() {
        let out = apply(&patch(None).unwrap(), Some(LIVE));
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc["models"].get("default").is_none(), "{out}");
        assert_eq!(
            doc["models"]["default_reasoning_effort"].as_str(),
            Some("high")
        );
        assert!(doc["model"].get("confdeck-old").is_none(), "{out}");
        assert!(doc["model"].get("mine").is_some(), "{out}");
        assert_eq!(read(&out), Connection::Default);

        // 在 Grok 里选了内置模型：留着，只删 confdeck 剩下的表和改道。
        let built_in = "[models]\ndefault = \"grok-4.6\"\n\n[model.confdeck-old]\nmodel = \"old\"\nbase_url = \"https://old.example/v1\"\napi_key = \"sk-old\"\n\n[endpoints]\nmodels_base_url = \"https://gw.example/v1\"\n";
        assert_eq!(
            apply(&patch(None).unwrap(), Some(built_in)),
            "[models]\ndefault = \"grok-4.6\"\n"
        );

        let ours = apply(&patch(Some(&relay())).unwrap(), None);
        assert_eq!(apply(&patch(None).unwrap(), Some(&ours)), "");
        assert_eq!(apply(&patch(None).unwrap(), None), "");
    }

    #[test]
    fn what_was_written_reads_back_as_the_same_connection() {
        let out = apply(&patch(Some(&relay())).unwrap(), None);
        assert_eq!(
            read(&out),
            Connection::Endpoint {
                base_url: "https://relay.example/v1".into(),
                credential: Some(Credential::Key("sk-relay".into())),
            }
        );
    }

    #[test]
    fn connection_follows_grok_credential_rules() {
        assert_eq!(
            connection(Path::new("c.toml"), None).unwrap(),
            Connection::Default
        );
        assert_eq!(
            read("[models]\ndefault = \"grok-4.6\"\n"),
            Connection::Default
        );
        // 只改内置模型参数的表还是内置模型。
        assert_eq!(
            read("[models]\ndefault = \"grok-4.6\"\n\n[model.\"grok-4.6\"]\ntemperature = 0.5\n"),
            Connection::Default
        );
        // 给内置模型换了 Key。
        assert_eq!(
            read(
                "[models]\ndefault = \"grok-4.6\"\n\n[model.\"grok-4.6\"]\napi_key = \"xai-mine\"\n"
            ),
            Connection::Other { host: None }
        );
        assert_eq!(
            read(
                "[models]\ndefault = \"m\"\n\n[model.m]\nbase_url = \"https://gw.example/v1/\"\nenv_key = [\"\", \"GW_KEY\"]\n"
            ),
            Connection::Endpoint {
                base_url: "https://gw.example/v1".into(),
                credential: Some(Credential::EnvVar("GW_KEY".into())),
            }
        );
        assert_eq!(
            read(
                "[models]\ndefault = \"m\"\n\n[model.m]\nbase_url = \"https://gw.example/v1\"\nauth_provider = \"corp\"\n\n[auth_provider.corp]\ncommand = \"corp-token\"\n"
            ),
            Connection::Endpoint {
                base_url: "https://gw.example/v1".into(),
                credential: Some(Credential::Helper("corp-token".into())),
            }
        );
        // 地址、协议、Key 从共享的 [model_providers.<id>] 继承。
        assert_eq!(
            read(
                "[models]\ndefault = \"m\"\n\n[model_providers.gw]\nbase_url = \"https://gw.example/v1\"\napi_backend = \"messages\"\napi_key = \"sk-gw\"\n\n[model.m]\nmodel = \"claude\"\nmodel_provider = \"gw\"\n"
            ),
            Connection::Endpoint {
                base_url: "https://gw.example".into(),
                credential: Some(Credential::Key("sk-gw".into())),
            }
        );
        assert_eq!(
            read("[endpoints]\nmodels_base_url = \"https://gw.example/v1\"\n"),
            Connection::Endpoint {
                base_url: "https://gw.example/v1".into(),
                credential: None,
            }
        );
    }

    #[test]
    fn setup_collects_the_models_at_the_same_address() {
        let text = r#"[models]
default = "a"

[model.a]
model = "m-a"
base_url = "https://gw.example/v1"
api_key = "sk-gw"
api_backend = "responses"

[model.c]
model = "m-c"
base_url = "https://other.example/v1"
api_backend = "responses"

[model.b]
model = "m-b"
base_url = "https://gw.example/v1/"
api_backend = "responses"

[model.chat]
model = "m-chat"
base_url = "https://gw.example/v1"
"#;
        let setup = setup(Path::new("c.toml"), Some(text.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(setup.protocol, Protocol::OpenaiResponses);
        assert_eq!(setup.base_url, "https://gw.example/v1");
        assert_eq!(setup.key.as_deref(), Some("sk-gw"));
        assert_eq!(setup.models, ["m-a", "m-b"]);
        assert_eq!(setup.default_model.as_deref(), Some("m-a"));
        assert_eq!(
            super::setup(
                Path::new("c.toml"),
                Some(b"[models]\ndefault = \"grok-4.6\"\n")
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn overrides_reach_every_table_and_the_default_model_is_always_written() {
        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::GrokBuild,
            AgentOverride {
                default_model: Some("glm-4.5".into()),
                raw: Some(
                    json!({"reasoning_summary": "none", "api_backend": null, "extra_headers": {"X-Team": "infra"}})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..AgentOverride::default()
            },
        );
        let out = apply(&patch(Some(&provider)).unwrap(), None);
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["models"]["default"].as_str(), Some("confdeck-glm-4.5"));
        let tables = doc["model"].as_table_like().unwrap();
        assert_eq!(tables.len(), 3, "{out}");
        for (_, table) in tables.iter() {
            assert_eq!(table["reasoning_summary"].as_str(), Some("none"));
            assert!(table.get("api_backend").is_none());
            assert_eq!(table["extra_headers"]["X-Team"].as_str(), Some("infra"));
        }
    }

    #[test]
    fn the_protocol_and_the_key_are_checked() {
        let mut chat_only = relay();
        chat_only.endpoints =
            BTreeMap::from([(Protocol::OpenaiChat, "https://chat.example/v1".into())]);
        let out = apply(&patch(Some(&chat_only)).unwrap(), None);
        assert!(out.contains("api_backend = \"chat_completions\""), "{out}");

        // 只有 Anthropic 兼容地址的：走 messages，地址补上 /v1，读回来还是存的那个地址。
        let mut anthropic_only = relay();
        anthropic_only.endpoints =
            BTreeMap::from([(Protocol::Anthropic, "https://a.example".into())]);
        let out = apply(&patch(Some(&anthropic_only)).unwrap(), None);
        assert!(out.contains("api_backend = \"messages\""), "{out}");
        assert!(out.contains("base_url = \"https://a.example/v1\""), "{out}");
        assert_eq!(
            read(&out),
            Connection::Endpoint {
                base_url: "https://a.example".into(),
                credential: Some(Credential::Key("sk-relay".into())),
            }
        );

        let mut official = relay();
        official.endpoints =
            BTreeMap::from([(Protocol::Anthropic, "https://api.anthropic.com".into())]);
        assert!(matches!(
            patch(Some(&official)),
            Err(AdaptError::AnthropicOfficial { .. })
        ));
        let mut gemini_only = relay();
        gemini_only.endpoints = BTreeMap::from([(Protocol::Gemini, "https://g.example".into())]);
        assert!(matches!(
            patch(Some(&gemini_only)),
            Err(AdaptError::Incompatible { .. })
        ));

        let mut keyless = relay();
        keyless.key.clear();
        assert!(matches!(
            patch(Some(&keyless)),
            Err(AdaptError::MissingKey { .. })
        ));
    }
}
