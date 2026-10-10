//! Codex：`~/.codex/config.toml`。单选 agent，列表 + 默认：列表写进模型目录文件（[`catalog`]），
//! `model_catalog_json` 指向它；默认模型写 `model`。
//!
//! `model_context_window`、`model_auto_compact_token_limit` 是用户自己的设置，不写也不清：官方模型可以用它把窗口调大
//! （gpt-5.6-sol 默认 272K，最大 872K），Codex 会把它截到每个模型的上限。第三方模型的窗口写进模型目录。
//!
//! 第三方供应商写成 `model_provider = "custom"` 加 `[model_providers.custom]` 一整张表；
//! 「默认」删掉这两处，回到 Codex 内置的 openai（官方登录）。关键字段的清单和
//! `requires_openai_auth` 的规则移植自 ccs-lite 的 `live/floor.rs`、`live/project/codex.rs`。
//!
//! ccs-lite 切回官方时不删 `custom` 表、改成连不上的休眠形态，理由是删了旧会话续不上。
//! Codex 0.153.4 上试过：删了表，`codex exec resume` 照样续上原来走 `custom` 的会话，请求按当前配置发；
//! 没生效的 profile 选着不存在的表也能正常加载。所以这里直接删。

pub mod catalog;

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use super::{
    AdaptError, AgentKind, Connection, Credential, FileSetup, choose_protocol, default_model,
    list_models, require_key,
};
use crate::engine::LiveFile;
use crate::fsx;
use crate::fsx::Mode;
use crate::patch::toml::{self, TomlDocPatch, put_table, put_value, remove_key, shape_error};
use crate::patch::{Format, PatchError};
use crate::provider::{Modality, Model, Protocol, Provider, normalize_url};

const AGENT: AgentKind = AgentKind::Codex;

/// confdeck 写的路由表的 id。
pub const ROUTE_ID: &str = "custom";

/// 不许定义成 `[model_providers.<id>]` 的内置 id：有这几张表，Codex 0.148 起整份配置都不加载
/// （0.153.4 实测）。`amazon-bedrock` 允许改少数几个字段；id 区分大小写，`OpenAI` 是普通 id。
pub const RESERVED_IDS: &[&str] = &["openai", "ollama", "lmstudio"];

pub fn config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

/// 里面有第三方的 Key，按 0600 写。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: config_path(home),
        mode: Mode::Private,
        format: Format::Toml,
    }
}

/// Codex 存 ChatGPT 登录的文件。
pub fn auth_path(home: &Path) -> PathBuf {
    home.join(".codex").join("auth.json")
}

/// `auth.json` 里有没有 Codex 能直接用的登录（OAuth token 等），只有 `OPENAI_API_KEY` 不算。
/// 判断规则移植自 ccs-lite 的 `codex_auth_has_credential_login_material`。登录不一定存在这个文件里，
/// 见 [`has_login`]。
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

/// Codex 看不看得到 ChatGPT 登录。登录存在哪由 `config.toml` 的 `cli_auth_credentials_store` 决定：
/// 不写或 `file` 只存 `auth.json`，看 `login_on_disk`；`ephemeral` 只在进程里，从不落盘；
/// `keyring`、`auto` 可能存在系统钥匙串里，confdeck 不读钥匙串，当成有（同 ccs-lite）。
/// 别的值 Codex 拒绝加载整份配置，也当成有。
pub fn has_login(doc: &DocumentMut, login_on_disk: bool) -> bool {
    let Some(store) = doc.get("cli_auth_credentials_store") else {
        return login_on_disk;
    };
    match store.as_str() {
        Some("file") => login_on_disk,
        Some("ephemeral") => false,
        _ => true,
    }
}

/// 路由表有没有自己的 Key：`experimental_bearer_token` 或 `env_key`。`auth` 命令、`aws`、
/// `Authorization` 请求头不算：`requires_openai_auth = true` 时官方登录会盖掉请求头（ccs-lite 在 0.149 核实）。
fn own_credentials(table: &Table) -> bool {
    text(table.get("experimental_bearer_token")).is_some() || table.contains_key("env_key")
}

/// 第三方路由表的 `requires_openai_auth`。
///
/// 这个值不决定请求用什么凭据（表自己的 Key 优先），但决定登录界面：为 `true` 而 Codex 看不到登录，
/// 会卡在登录页；为 `false` 而旁边留着 ChatGPT 登录，Codex 当成已登出（账号信息不显示、token 不刷新）。
/// 所以表有自己的 Key 时跟着「有没有登录」走。没有自己的 Key 时恒为 `false`：为 `true` 时请求会带上
/// 官方登录，发给第三方地址。（ccs-lite 在 Codex 0.149 上核实；登录界面的两种表现 10-10 在 0.153.4 上又试过）
pub fn requires_openai_auth(own_credentials: bool, login: bool) -> bool {
    own_credentials && login
}

#[derive(Debug, Clone)]
pub struct CodexPatch {
    /// 顶层关键字段的目标值。
    top: Vec<(&'static str, Value)>,
    /// `[model_providers.custom]`；`None` 是「默认」。
    route: Option<Route>,
    /// `~/.codex/auth.json` 里有没有 ChatGPT 登录。
    login_on_disk: bool,
}

#[derive(Debug, Clone)]
struct Route {
    table: Table,
    /// 原始片段没写 `requires_openai_auth`，由 confdeck 在写入时定值。
    stamp: bool,
}

/// 切到这家供应商的补丁；`None` 是「默认」。`login_on_disk`：`~/.codex/auth.json`
/// 里有没有 ChatGPT 登录。模型目录文件写好了再加上 [`CodexPatch::with_catalog`]。
pub fn patch(provider: Option<&Provider>, login_on_disk: bool) -> Result<CodexPatch, AdaptError> {
    let Some(provider) = provider else {
        return Ok(CodexPatch {
            top: Vec::new(),
            route: None,
            login_on_disk,
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
    // 先占住位置，值要看 `config.toml` 里登录存在哪，写入时再定。
    table.insert("requires_openai_auth", toml_edit::value(false));
    let raw = provider.override_for(AGENT).and_then(|o| o.raw.as_ref());
    for (name, value) in raw.into_iter().flatten() {
        if value.is_null() {
            table.remove(name);
            continue;
        }
        let value = toml::from_json(value).ok_or_else(|| AdaptError::RawNotToml {
            agent: AGENT,
            key: name.clone(),
        })?;
        table.insert(name, Item::Value(value));
    }
    // 占位的值是 false，这里是 true 只能是原始片段写的。
    if table.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
        && !own_credentials(&table)
    {
        return Err(AdaptError::RawSendsLogin { agent: AGENT });
    }
    let stamp = !raw.is_some_and(|raw| raw.contains_key("requires_openai_auth"));

    Ok(CodexPatch {
        top: vec![
            ("model_provider", Value::from(ROUTE_ID)),
            ("model", Value::from(model.id.as_str())),
        ],
        route: Some(Route { table, stamp }),
        login_on_disk,
    })
}

impl CodexPatch {
    /// `model_catalog_json` 指向 confdeck 的模型目录文件。目录要先写好：指着不存在的文件 Codex 起不来。
    pub fn with_catalog(mut self) -> Self {
        if self.route.is_some() {
            self.top
                .push(("model_catalog_json", Value::from(catalog::FILE_NAME)));
        }
        self
    }
}

impl TomlDocPatch for CodexPatch {
    fn apply_to(&self, path: &Path, doc: &mut DocumentMut) -> Result<(), PatchError> {
        let login = has_login(doc, self.login_on_disk);

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
        let route = self.route.as_ref().map(|route| {
            let mut table = route.table.clone();
            if route.stamp {
                let value = requires_openai_auth(own_credentials(&table), login);
                table.insert("requires_openai_auth", toml_edit::value(value));
            }
            table
        });
        write_route(path, doc, route)?;

        // 4. 目标值：已有就原位改，没有就追加。
        for (key, value) in &self.top {
            put_value(doc.as_table_mut(), key, value);
        }

        // 5. 生效的 profile 自己选了别的路由，写了 Codex 也不走 `custom`。「默认」不查：删掉自己的表总是对的。
        if self.route.is_some()
            && let Some((name, profile)) = active_profile(doc)
            && text(profile.get("model_provider")).is_some_and(|selected| selected != ROUTE_ID)
        {
            return Err(PatchError::Overridden {
                path: path.to_path_buf(),
                by: format!("生效的 profile「{name}」"),
                key: "model_provider".to_string(),
            });
        }
        Ok(())
    }
}

/// 旧写法的 profile：顶层 `profile = "名字"` 选中 `[profiles.名字]`，里面的键盖过顶层的。
/// Codex 0.153.4 已经不认（有顶层 `profile` 就报错退出；profile 改成单独的 `名字.config.toml`，
/// 只在命令行带 `--profile` 时生效），更早的版本还认。
fn active_profile(doc: &DocumentMut) -> Option<(String, &dyn TableLike)> {
    let name = text(doc.get("profile"))?;
    let profile = doc.get("profiles")?.get(&name)?.as_table_like()?;
    Some((name, profile))
}

/// 键的生效值：生效的 profile 里有就用它的。
fn effective<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    active_profile(doc)
        .and_then(|(_, profile)| profile.get(key))
        .or_else(|| doc.get(key))
}

fn text(item: Option<&Item>) -> Option<String> {
    item.and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn write_route(path: &Path, doc: &mut DocumentMut, route: Option<Table>) -> Result<(), PatchError> {
    let root = doc.as_table_mut();
    let container_inline = matches!(root.get("model_providers"), Some(Item::Value(_)));
    let Some(container) = root.get_mut("model_providers") else {
        if let Some(table) = route {
            // 隐式表：只输出 `[model_providers.custom]`，不多一行空的 `[model_providers]`。
            let mut providers = Table::new();
            providers.set_implicit(true);
            providers.insert(ROUTE_ID, Item::Table(table));
            root.insert("model_providers", Item::Table(providers));
        }
        return Ok(());
    };
    let providers = container
        .as_table_like_mut()
        .ok_or_else(|| shape_error(path, &["model_providers".to_string()]))?;
    rename_reserved(providers);
    match route {
        Some(table) => put_table(providers, ROUTE_ID, table, container_inline),
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

/// 保留 id 的表改名成 `<id>-custom`（Codex 报错里建议的名字），内容不动。这几张表不是 confdeck 写的，
/// 删了可能丢掉用户要的设置；不改 Codex 又整份不加载。
fn rename_reserved(providers: &mut dyn TableLike) {
    for id in RESERVED_IDS {
        if !providers.get(id).is_some_and(Item::is_table_like) {
            continue;
        }
        let item = providers.remove(id).expect("checked above");
        let base = format!("{id}-custom");
        let mut renamed = base.clone();
        let mut suffix = 2;
        while providers.contains_key(&renamed) {
            renamed = format!("{base}-{suffix}");
            suffix += 1;
        }
        providers.insert(&renamed, item);
    }
}

/// 写进模型目录的模型：列表（覆盖里选的，或打星的），默认模型不在列表里时排在最前面。
pub fn catalog_models(provider: &Provider) -> Result<Vec<&Model>, AdaptError> {
    let mut models = list_models(provider, AGENT)?;
    let default = default_model(provider, AGENT)?;
    if !models.iter().any(|model| model.id == default.id) {
        models.insert(0, default);
    }
    Ok(models)
}

/// `config.toml` 的 `model_catalog_json` 指向的文件。相对路径按配置文件所在目录（`CODEX_HOME`）算。
fn catalog_path(path: &Path, doc: &DocumentMut) -> Option<PathBuf> {
    let value = text(doc.get("model_catalog_json"))?;
    let dir = path.parent().unwrap_or(Path::new(""));
    Some(dir.join(value))
}

/// 现在的 `config.toml` 指着 confdeck 的模型目录文件：改了供应商的模型以后要跟着重写目录。
pub fn uses_catalog(path: &Path, bytes: Option<&[u8]>) -> bool {
    toml::parse(path, bytes)
        .is_ok_and(|doc| text(doc.get("model_catalog_json")).as_deref() == Some(catalog::FILE_NAME))
}

/// 模型目录文件里的模型（confdeck 写的，或别的工具写的）：ID、显示名、窗口、图片输入、推理档位。读不了为空。
fn catalog_entries(path: &Path) -> Vec<Model> {
    let catalog: Option<Json> = fsx::read_optional(path)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let entries = catalog
        .as_ref()
        .and_then(|catalog| catalog.get("models"))
        .and_then(Json::as_array);
    entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("slug")?.as_str()?.trim();
            let text = |key: &str| entry.get(key).and_then(Json::as_str);
            let name = text("display_name").filter(|name| *name != id);
            let context = ["context_window", "max_context_window"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(Json::as_u64))
                .filter(|&context| context > 0);
            let image = entry
                .get("input_modalities")
                .and_then(Json::as_array)
                .is_some_and(|input| input.iter().any(|m| m == "image"));
            let reasoning = entry
                .get("supported_reasoning_levels")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|level| level.get("effort")?.as_str())
                .map(str::to_string)
                .collect();
            Some(Model {
                id: id.to_string(),
                name: name.map(str::to_string),
                context,
                input: if image {
                    vec![Modality::Text, Modality::Image]
                } else {
                    Vec::new()
                },
                reasoning,
                ..Model::default()
            })
            .filter(|model| !model.id.is_empty())
        })
        .collect()
}

/// 从 `config.toml` 读出现在连的是哪。只看选的路由和它的地址、凭据，不看模型名。
/// 旧写法的 profile 生效时按它盖过的值算。
pub fn connection(path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
    let doc = toml::parse(path, bytes)?;
    let selected = text(effective(&doc, "model_provider")).unwrap_or_else(|| "openai".to_string());
    if selected == "openai" {
        // `openai_base_url` 把内置的 openai 路由改道到别的地址。
        return Ok(match text(effective(&doc, "openai_base_url")) {
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
        .map(Credential::Key)
        .or_else(|| text(table.get("env_key")).map(Credential::EnvVar));
    Ok(Connection::Endpoint {
        base_url: normalize_url(Protocol::OpenaiResponses, &base_url),
        credential,
    })
}

/// 「把当前配置存成供应商」：读出地址、Key、`model`，和模型目录文件里的模型。连的不是一个地址时为 `None`。
pub fn setup(path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
    let Connection::Endpoint {
        base_url,
        credential,
    } = connection(path, bytes)?
    else {
        return Ok(None);
    };
    let doc = toml::parse(path, bytes)?;
    let model = text(effective(&doc, "model"));
    let mut models = catalog_path(path, &doc)
        .map(|catalog| catalog_entries(&catalog))
        .unwrap_or_default();
    if let Some(id) = &model
        && !models.iter().any(|known| known.id == *id)
    {
        models.insert(
            0,
            Model {
                id: id.clone(),
                ..Model::default()
            },
        );
    }
    Ok(Some(FileSetup {
        protocol: Protocol::OpenaiResponses,
        base_url,
        key: credential
            .as_ref()
            .and_then(Credential::key)
            .map(str::to_string),
        models,
        slots: Default::default(),
        default_model: model,
    }))
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
                credential: Some(Credential::EnvVar("OLLAMA_KEY".into()))
            }
        );
        assert_eq!(
            read("model_provider = \"ollama\"\n"),
            Connection::Other { host: None }
        );
    }

    #[test]
    fn setup_reads_the_route_and_the_model() {
        let text = "model = \"glm-4.6\"\nmodel_provider = \"relay\"\n\n[model_providers.relay]\nbase_url = \"https://relay.elsewhere.io/v1\"\nexperimental_bearer_token = \"sk-x\"\n";
        let read = setup(Path::new("c.toml"), Some(text.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(read.protocol, Protocol::OpenaiResponses);
        assert_eq!(read.base_url, "https://relay.elsewhere.io/v1");
        assert_eq!(read.key.as_deref(), Some("sk-x"));
        assert_eq!(
            read.models,
            [Model {
                id: "glm-4.6".into(),
                ..Model::default()
            }]
        );
        assert_eq!(read.default_model.as_deref(), Some("glm-4.6"));
        assert!(read.slots.is_empty());
        assert_eq!(
            setup(Path::new("c.toml"), Some(LIVE.as_bytes())).unwrap(),
            None
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
                credential: Some(Credential::Key("sk-relay".into()))
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

    #[test]
    fn the_catalog_pointer_comes_and_goes_with_the_route() {
        let with = apply(&patch(Some(&relay()), false).unwrap().with_catalog(), None);
        assert!(
            with.contains("model_catalog_json = \"confdeck-models.json\""),
            "{with}"
        );
        assert!(uses_catalog(
            Path::new("config.toml"),
            Some(with.as_bytes())
        ));
        // 换一家没写目录的、切回默认，都清掉。
        let without = apply(&patch(Some(&relay()), false).unwrap(), Some(&with));
        assert!(!without.contains("model_catalog_json"), "{without}");
        assert!(
            !apply(&patch(None, false).unwrap().with_catalog(), Some(&with))
                .contains("model_catalog_json")
        );
        assert!(!uses_catalog(
            Path::new("config.toml"),
            Some(b"model_catalog_json = \"cc-switch-model-catalog.json\"\n")
        ));
    }

    #[test]
    fn setup_reads_the_models_from_the_catalog_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            dir.path().join("cc-switch-model-catalog.json"),
            serde_json::to_vec(&json!({"models": [
                {"slug": "glm-5.3", "display_name": "GLM-5.3", "context_window": 1048576,
                 "input_modalities": ["text"],
                 "supported_reasoning_levels": [{"effort": "low"}, {"effort": "max"}]},
                {"slug": "glm-5.3-flash", "display_name": "glm-5.3-flash",
                 "input_modalities": ["text", "image"]}
            ]}))
            .unwrap(),
        )
        .unwrap();
        let text = "model = \"glm-5-turbo\"\nmodel_provider = \"custom\"\nmodel_catalog_json = \"cc-switch-model-catalog.json\"\n\n[model_providers.custom]\nbase_url = \"https://open.bigmodel.cn/api/v1\"\nexperimental_bearer_token = \"sk\"\n";
        let found = setup(&config, Some(text.as_bytes())).unwrap().unwrap();
        assert_eq!(
            found.models,
            [
                Model {
                    id: "glm-5-turbo".into(),
                    ..Model::default()
                },
                Model {
                    id: "glm-5.3".into(),
                    name: Some("GLM-5.3".into()),
                    context: Some(1_048_576),
                    reasoning: vec!["low".into(), "max".into()],
                    ..Model::default()
                },
                Model {
                    id: "glm-5.3-flash".into(),
                    input: vec![Modality::Text, Modality::Image],
                    ..Model::default()
                },
            ]
        );
        assert_eq!(found.default_model.as_deref(), Some("glm-5-turbo"));
    }

    #[test]
    fn the_catalog_lists_the_default_model_first_when_it_is_not_starred() {
        let mut provider = relay();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                default_model: Some("glm-4.5".into()),
                ..AgentOverride::default()
            },
        );
        let ids: Vec<&str> = catalog_models(&provider)
            .unwrap()
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(ids, ["glm-4.5", "glm-4.5-air", "glm-4.6"]);
    }

    #[test]
    fn the_users_window_settings_survive_every_switch() {
        let pre = "model = \"gpt-5.6-sol\"\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 800000\n";
        let third = apply(&patch(Some(&relay()), false).unwrap(), Some(pre));
        let back = apply(&patch(None, false).unwrap(), Some(&third));
        for out in [&third, &back] {
            assert!(out.contains("model_context_window = 1000000"), "{out}");
            assert!(
                out.contains("model_auto_compact_token_limit = 800000"),
                "{out}"
            );
        }
    }

    #[test]
    fn reserved_tables_are_renamed_on_every_write() {
        let pre = r#"model_provider = "openai"

# 以前的代理
[model_providers.openai]
name = "Proxy"
base_url = "https://proxy.example/v1"

[model_providers.openai-custom]
name = "Taken"

[model_providers.OpenAI]
name = "Not reserved"
"#;
        assert_eq!(
            apply(&patch(None, false).unwrap(), Some(pre)),
            r#"# 以前的代理
[model_providers.openai-custom-2]
name = "Proxy"
base_url = "https://proxy.example/v1"

[model_providers.openai-custom]
name = "Taken"

[model_providers.OpenAI]
name = "Not reserved"
"#
        );

        let pre = "model_providers = { ollama = { base_url = \"http://localhost:11434/v1\" } }\n";
        let out = apply(&patch(Some(&relay()), false).unwrap(), Some(pre));
        let doc: DocumentMut = out.parse().unwrap();
        let providers = doc["model_providers"].as_inline_table().unwrap();
        assert!(!providers.contains_key("ollama"), "{out}");
        assert!(providers.contains_key("ollama-custom"), "{out}");
        assert!(providers.contains_key(ROUTE_ID), "{out}");
    }

    fn requires(pre: &str, login_on_disk: bool) -> Option<bool> {
        let out = apply(&patch(Some(&relay()), login_on_disk).unwrap(), Some(pre));
        let doc: DocumentMut = out.parse().unwrap();
        doc["model_providers"][ROUTE_ID]
            .get("requires_openai_auth")
            .and_then(Item::as_bool)
    }

    #[test]
    fn requires_openai_auth_follows_where_codex_keeps_the_login() {
        for (store, without, with) in [
            ("", false, true),
            ("cli_auth_credentials_store = \"file\"\n", false, true),
            ("cli_auth_credentials_store = \"ephemeral\"\n", false, false),
            ("cli_auth_credentials_store = \"keyring\"\n", true, true),
            ("cli_auth_credentials_store = \"auto\"\n", true, true),
        ] {
            assert_eq!(requires(store, false), Some(without), "{store}");
            assert_eq!(requires(store, true), Some(with), "{store}");
        }
    }

    fn with_raw(raw: Json) -> Provider {
        let mut provider = relay();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                raw: Some(raw.as_object().unwrap().clone()),
                ..AgentOverride::default()
            },
        );
        provider
    }

    #[test]
    fn a_table_without_its_own_key_never_asks_for_the_login() {
        // 改用请求头带 Key：不能要求官方登录，否则登录会被发给第三方。
        let provider = with_raw(json!({
            "experimental_bearer_token": null,
            "http_headers": {"Authorization": "Bearer sk-relay"}
        }));
        let out = apply(&patch(Some(&provider), true).unwrap(), None);
        assert!(out.contains("requires_openai_auth = false"), "{out}");

        let provider = with_raw(json!({"env_key": "RELAY_KEY", "experimental_bearer_token": null}));
        let out = apply(&patch(Some(&provider), true).unwrap(), None);
        assert!(out.contains("requires_openai_auth = true"), "{out}");

        let provider = with_raw(json!({
            "experimental_bearer_token": null,
            "requires_openai_auth": true
        }));
        assert!(matches!(
            patch(Some(&provider), true),
            Err(AdaptError::RawSendsLogin { .. })
        ));

        // 有自己的 Key 时，原始片段写的值照写。
        let provider = with_raw(json!({"requires_openai_auth": false}));
        let out = apply(&patch(Some(&provider), true).unwrap(), None);
        assert!(out.contains("requires_openai_auth = false"), "{out}");
    }

    const PROFILES: &str = r#"profile = "work"

[profiles.work]
model_provider = "azure"
model = "gpt-5.5"

[profiles.home]
model_provider = "custom"
"#;

    #[test]
    fn an_active_profile_on_another_route_is_refused() {
        let err = patch(Some(&relay()), false)
            .unwrap()
            .apply(Path::new("config.toml"), Some(PROFILES.as_bytes()))
            .unwrap_err();
        assert!(matches!(err, PatchError::Overridden { .. }), "{err:?}");

        // 「默认」照写；生效的 profile 选的是 custom、或者 profile 没生效，都照写。
        apply(&patch(None, false).unwrap(), Some(PROFILES));
        let home = PROFILES.replace("profile = \"work\"", "profile = \"home\"");
        apply(&patch(Some(&relay()), false).unwrap(), Some(&home));
        let inactive = PROFILES.replace("profile = \"work\"\n", "");
        apply(&patch(Some(&relay()), false).unwrap(), Some(&inactive));
    }

    #[test]
    fn connection_and_setup_follow_the_active_profile() {
        let text = format!(
            "{PROFILES}\n[model_providers.azure]\nbase_url = \"https://azure.example/openai/v1\"\nenv_key = \"AZURE_KEY\"\n"
        );
        assert_eq!(
            read(&text),
            Connection::Endpoint {
                base_url: "https://azure.example/openai/v1".into(),
                credential: Some(Credential::EnvVar("AZURE_KEY".into()))
            }
        );
        let found = setup(Path::new("c.toml"), Some(text.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(found.default_model.as_deref(), Some("gpt-5.5"));
    }
}
