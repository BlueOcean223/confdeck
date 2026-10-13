//! core 的错误在界面上怎么说（arch-perf R6）。core 的错误只有中文的 `Display`，界面上不用它：每种错误的每个变体都在这里
//! 按界面语言说。`match` 不写 `_`，core 加了变体这里编不过。路径、行列、解析器和系统的原话照旧带上。
//!
//! 中文和 core 的原话一样；联网、额度、余额、登录、Codex 模型目录几种原来就用 app 自己的说法，照旧。
//! core 有几处把中文写进了错误的字段里（Codex 生效的 profile、不是 UTF-8、「对象」「表」「字符串」），这里按原话认出来再说；
//! 测试从 core 真会走的路径造出这几种错误，core 改了说法测试不过。
//!
//! 一般用 [`Describe::describe`]；同一个错误在某处要说得短一点的（编辑框里不用再说路径、编辑器里不用再说是哪家），
//! 包一层再说，比如 `InRaw(&err).describe()`。

use std::borrow::Cow;
use std::io;
use std::path::Path;

use confdeck_core::account::AccountError;
use confdeck_core::agent::codex::catalog::Skipped;
use confdeck_core::agent::{AdaptError, ApplyError, Mode};
use confdeck_core::balance::BalanceError;
use confdeck_core::engine::EngineError;
use confdeck_core::net::{NetError, Unreachable};
use confdeck_core::patch::PatchError;
use confdeck_core::provider::{Protocol, ProviderError};
use confdeck_core::quota::QuotaError;
use confdeck_core::store::StoreError;
use confdeck_core::usage::UsageError;
use rust_i18n::t;

pub(crate) trait Describe {
    /// 按 `locale` 说的一句话。
    fn describe_in(&self, locale: &str) -> String;

    /// 按界面语言说的一句话。
    fn describe(&self) -> String {
        self.describe_in(&rust_i18n::locale())
    }
}

impl Describe for io::Error {
    /// 系统给的原话。core 自己造的几句是中文（`fsx` 的软链接层数太多、路径没有文件名，钥匙串的名字里有引号），都碰不太到。
    fn describe_in(&self, _locale: &str) -> String {
        self.to_string()
    }
}

impl Describe for EngineError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            EngineError::Io { path, source } => io(l, path, source),
            EngineError::Patch(err) => err.describe_in(l),
            EngineError::Conflict { path } => {
                t!("errors.conflict", locale = l, path = path.display()).into()
            }
        }
    }
}

impl Describe for PatchError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            PatchError::Parse {
                path,
                line,
                column,
                message,
            } => parse(l, path, *line, *column, message),
            PatchError::Shape {
                path,
                key_path,
                expected,
            } => t!(
                "errors.shape",
                locale = l,
                path = path.display(),
                key = key_path,
                expected = shape(l, expected)
            )
            .into(),
            PatchError::Empty { path } => {
                t!("errors.empty_json", locale = l, path = path.display()).into()
            }
            PatchError::Overridden { path, by, key } => t!(
                "errors.overridden",
                locale = l,
                path = path.display(),
                by = overrider(l, by),
                key = key
            )
            .into(),
        }
    }
}

impl Describe for AdaptError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            AdaptError::Incompatible {
                agent,
                provider,
                supported,
            } => t!(
                "errors.incompatible",
                locale = l,
                provider = provider,
                agent = agent,
                supported = list(l, supported)
            ),
            AdaptError::NoModels { agent, provider } => t!(
                "errors.no_models",
                locale = l,
                provider = provider,
                agent = agent
            ),
            AdaptError::UnknownModel { provider, model } => t!(
                "errors.missing_model",
                locale = l,
                provider = provider,
                model = model
            ),
            AdaptError::UnknownSlot { agent, slot } => {
                t!(
                    "errors.unknown_slot",
                    locale = l,
                    agent = agent,
                    slot = slot
                )
            }
            AdaptError::MissingKey { agent, provider } => t!(
                "errors.missing_key",
                locale = l,
                provider = provider,
                agent = agent
            ),
            AdaptError::RawShape {
                agent,
                key,
                expected,
            } => t!(
                "errors.raw_shape",
                locale = l,
                agent = agent,
                key = key,
                expected = shape(l, expected)
            ),
            AdaptError::RawNotToml { agent, key } => {
                t!("errors.raw_not_toml", locale = l, agent = agent, key = key)
            }
            AdaptError::RawSendsLogin { agent } => {
                t!("errors.raw_sends_login", locale = l, agent = agent)
            }
            AdaptError::AnthropicOfficial { agent } => {
                t!("errors.anthropic_official", locale = l, agent = agent)
            }
        }
        .into()
    }
}

impl Describe for ApplyError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            ApplyError::Adapt(err) => err.describe_in(l),
            ApplyError::Engine(err) => err.describe_in(l),
            ApplyError::Parse(err) => err.describe_in(l),
            ApplyError::WrongMode { agent } => match agent.adapter().mode() {
                Mode::Switch => t!("errors.wrong_mode_switch", locale = l, agent = agent),
                Mode::Coexist => t!("errors.wrong_mode_coexist", locale = l, agent = agent),
            }
            .into(),
        }
    }
}

impl Describe for StoreError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            StoreError::Io { path, source } => io(l, path, source),
            StoreError::Parse {
                path,
                line,
                column,
                message,
            } => parse(l, path, *line, *column, message),
            StoreError::Version { path, found } => t!(
                "errors.newer_version",
                locale = l,
                path = path.display(),
                found = found
            )
            .into(),
            StoreError::Invalid { path, source } => t!(
                "errors.invalid_data",
                locale = l,
                path = path.display(),
                error = source.describe_in(l)
            )
            .into(),
            StoreError::Lock(err) => err.describe_in(l),
        }
    }
}

impl Describe for ProviderError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            ProviderError::BadId { id } => t!("errors.provider_bad_id", locale = l, id = id),
            ProviderError::DuplicateId { id } => {
                t!("errors.provider_duplicate_id", locale = l, id = id)
            }
            ProviderError::EmptyName { id } => {
                t!("errors.provider_empty_name", locale = l, id = id)
            }
            ProviderError::NoEndpoint { id } => {
                t!("errors.provider_no_endpoint", locale = l, id = id)
            }
            ProviderError::BadUrl { id, protocol, url } => t!(
                "errors.provider_bad_url",
                locale = l,
                id = id,
                protocol = protocol,
                url = url
            ),
            ProviderError::EmptyModelId { id } => {
                t!("errors.provider_empty_model_id", locale = l, id = id)
            }
            ProviderError::DuplicateModel { id, model } => t!(
                "errors.provider_duplicate_model",
                locale = l,
                id = id,
                model = model
            ),
            ProviderError::UnknownModel { id, agent, model } => t!(
                "errors.provider_unknown_model",
                locale = l,
                id = id,
                agent = agent,
                model = model
            ),
            ProviderError::UnknownDefaultModel { id, model } => t!(
                "errors.provider_unknown_default_model",
                locale = l,
                id = id,
                model = model
            ),
        }
        .into()
    }
}

impl Describe for AccountError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            AccountError::Engine(err) => err.describe_in(l),
            AccountError::Unsupported => t!("errors.account_unsupported", locale = l).into(),
            AccountError::Missing => t!("errors.account_missing", locale = l).into(),
            AccountError::Unrecognized => t!("accounts.unrecognized", locale = l).into(),
            AccountError::NoProgram(name) => {
                t!("accounts.no_program", locale = l, program = name).into()
            }
            AccountError::Login(output) => {
                t!("accounts.login_failed", locale = l, error = output).into()
            }
            AccountError::LoginTimedOut => t!("accounts.login_timed_out", locale = l).into(),
        }
    }
}

impl Describe for NetError {
    /// 联网出错的一句话（连通结果、代理测试、拉列表、额度都用它）。
    fn describe_in(&self, l: &str) -> String {
        match self {
            NetError::BadProxy { .. } => t!("net.bad_proxy", locale = l),
            NetError::BadUrl { .. } => t!("net.bad_url", locale = l),
            NetError::Timeout => t!("net.timeout", locale = l),
            NetError::Unreachable(Unreachable::Dns) => t!("net.dns", locale = l),
            NetError::Unreachable(Unreachable::Refused) => t!("net.refused", locale = l),
            NetError::Unreachable(Unreachable::Tls) => t!("net.tls", locale = l),
            NetError::Unreachable(Unreachable::Proxy) => t!("net.proxy", locale = l),
            NetError::Unreachable(Unreachable::Closed) => t!("net.closed", locale = l),
            NetError::Unreachable(Unreachable::Other(detail)) => {
                t!("net.unreachable", locale = l, detail = detail)
            }
            NetError::Status {
                status: status @ (401 | 403),
                ..
            } => t!("net.key_rejected", locale = l, status = status),
            NetError::Status { status, .. } => t!("net.status", locale = l, status = status),
            NetError::Parse { .. } => t!("net.parse", locale = l),
        }
        .into()
    }
}

impl Describe for QuotaError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            QuotaError::NotLoggedIn => t!("quota.signed_out", locale = l).into(),
            QuotaError::Expired => t!("quota.expired", locale = l).into(),
            QuotaError::Auth => t!("quota.key_rejected", locale = l).into(),
            QuotaError::NoPlan => t!("quota.none", locale = l).into(),
            QuotaError::Net(err) => err.describe_in(l),
            QuotaError::Parse(_) => t!("quota.unknown", locale = l).into(),
        }
    }
}

impl Describe for BalanceError {
    fn describe_in(&self, l: &str) -> String {
        match self {
            BalanceError::NoKey => t!("providers.no_key", locale = l).into(),
            BalanceError::Auth => t!("quota.key_rejected", locale = l).into(),
            BalanceError::Net(err) => err.describe_in(l),
            BalanceError::Parse(_) => t!("quota.unknown", locale = l).into(),
        }
    }
}

impl Describe for UsageError {
    /// 路径加上 SQLite 或系统的原话。
    fn describe_in(&self, l: &str) -> String {
        match self {
            UsageError::Db { path, source } => format!("{}: {source}", path.display()),
            UsageError::Io { path, source } => {
                format!("{}: {}", path.display(), source.describe_in(l))
            }
        }
    }
}

impl Describe for Skipped {
    /// Codex 模型目录没生成出来的原因。
    fn describe_in(&self, l: &str) -> String {
        match self {
            Skipped::NotFound => t!("codex.not_found", locale = l),
            Skipped::Failed => t!("codex.failed", locale = l),
            Skipped::NoTemplate => t!("codex.no_template", locale = l),
        }
        .into()
    }
}

/// 拉模型列表出错：404 说的是「这家没有列表接口」，别的同 [`Describe`]。
#[derive(Debug)]
pub(crate) struct Listing<'a>(pub &'a NetError);

impl Describe for Listing<'_> {
    fn describe_in(&self, l: &str) -> String {
        match self.0 {
            NetError::Status {
                status: 404 | 405, ..
            } => t!("net.no_model_list", locale = l).into(),
            other => other.describe_in(l),
        }
    }
}

/// 编辑器里的校验错误：说的就是正在编辑的这家，不带 ID。
#[derive(Debug)]
pub(crate) struct InEditor<'a>(pub &'a ProviderError);

impl Describe for InEditor<'_> {
    fn describe_in(&self, l: &str) -> String {
        match self.0 {
            ProviderError::BadId { .. } => t!("errors.bad_id", locale = l),
            ProviderError::DuplicateId { id } => t!("errors.duplicate_id", locale = l, id = id),
            ProviderError::EmptyName { .. } => t!("errors.empty_name", locale = l),
            ProviderError::NoEndpoint { .. } => t!("errors.no_endpoint", locale = l),
            ProviderError::BadUrl { protocol, url, .. } => {
                t!("errors.bad_url", locale = l, protocol = protocol, url = url)
            }
            ProviderError::EmptyModelId { .. } => t!("errors.empty_model_id", locale = l),
            ProviderError::DuplicateModel { model, .. } => {
                t!("errors.duplicate_model", locale = l, model = model)
            }
            ProviderError::UnknownModel { agent, model, .. } => {
                t!(
                    "errors.unknown_model",
                    locale = l,
                    agent = agent,
                    model = model
                )
            }
            ProviderError::UnknownDefaultModel { model, .. } => {
                t!("errors.unknown_default_model", locale = l, model = model)
            }
        }
        .into()
    }
}

/// 「原始文件」编辑框里的：解析不了时只说第几行第几列（编辑框里就是这个文件，不用再写路径），别的同 [`Describe`]。
#[derive(Debug)]
pub(crate) struct InRaw<'a>(pub &'a PatchError);

impl Describe for InRaw<'_> {
    fn describe_in(&self, l: &str) -> String {
        match self.0 {
            PatchError::Parse {
                line,
                column,
                message,
                ..
            } => t!(
                "config.parse_error",
                locale = l,
                line = line,
                column = column,
                message = without_position(&says(l, message), *line, *column)
            )
            .into(),
            other => other.describe_in(l),
        }
    }
}

/// 「原始文件」保存没写成：被别处改过了说没有保存，解析不了同 [`InRaw`]。
#[derive(Debug)]
pub(crate) struct InRawWrite<'a>(pub &'a EngineError);

impl Describe for InRawWrite<'_> {
    fn describe_in(&self, l: &str) -> String {
        match self.0 {
            EngineError::Patch(err) => InRaw(err).describe_in(l),
            EngineError::Conflict { .. } => t!("config.conflict", locale = l).into(),
            other => other.describe_in(l),
        }
    }
}

/// 导入的文件读不了：格式不对只说第几行，是新版本导出的不说路径（用户刚选的文件），别的同 [`Describe`]。
#[derive(Debug)]
pub(crate) struct Importing<'a>(pub &'a StoreError);

impl Describe for Importing<'_> {
    fn describe_in(&self, l: &str) -> String {
        match self.0 {
            StoreError::Parse { line, .. } => {
                t!("transfer.bad_file", locale = l, line = line).into()
            }
            StoreError::Version { .. } => t!("transfer.too_new", locale = l).into(),
            other => other.describe_in(l),
        }
    }
}

fn io(l: &str, path: &Path, source: &io::Error) -> String {
    t!(
        "errors.io",
        locale = l,
        path = path.display(),
        error = source.describe_in(l)
    )
    .into()
}

/// 解析不了。中文和 core 的原话一样；英文去掉解析器原话末尾的「at line 1 column 1」，前面已经说了。
fn parse(l: &str, path: &Path, line: usize, column: usize, message: &str) -> String {
    let message = says(l, message);
    let message = if l.starts_with("zh") {
        &message
    } else {
        without_position(&message, line, column)
    };
    t!(
        "errors.parse",
        locale = l,
        path = path.display(),
        line = line,
        column = column,
        message = message
    )
    .into()
}

/// 解析错误里的原话：解析器的照原样（英文），core 自己写的那句按界面语言说。
fn says<'a>(l: &str, message: &'a str) -> Cow<'a, str> {
    match message {
        "不是 UTF-8 文本" => t!("errors.not_utf8", locale = l),
        other => Cow::Borrowed(other),
    }
}

/// serde 的原话末尾自带「at line 1 column 1」。
fn without_position(message: &str, line: usize, column: usize) -> &str {
    message
        .strip_suffix(&format!(" at line {line} column {column}"))
        .unwrap_or(message)
}

/// [`PatchError::Overridden`] 里盖过要写的值的那个：Codex 生效的 profile（core 写的是「生效的 profile「名字」」）。
fn overrider(l: &str, by: &str) -> String {
    match by
        .strip_prefix("生效的 profile「")
        .and_then(|rest| rest.strip_suffix('」'))
    {
        Some(name) => t!("errors.active_profile", locale = l, name = name).into(),
        None => by.to_string(),
    }
}

/// core 写在错误里的「应该是什么」（对象、表、字符串）；不认识的照原样。
fn shape(l: &str, expected: &str) -> Cow<'static, str> {
    match expected {
        "对象" => t!("errors.an_object", locale = l),
        "表" => t!("errors.a_table", locale = l),
        "字符串" => t!("errors.a_string", locale = l),
        other => other.to_string().into(),
    }
}

fn list(l: &str, protocols: &[Protocol]) -> String {
    protocols
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(&t!("errors.list_separator", locale = l))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use confdeck_core::agent::{AgentKind, claude, codex};
    use confdeck_core::patch::{self, Format, LivePatch as _};
    use confdeck_core::provider::{AgentOverride, Model, Provider};
    use confdeck_core::usage::Db;
    use serde_json::json;

    use super::*;

    const EN: &str = "en";
    const ZH: &str = "zh-CN";

    fn path() -> PathBuf {
        PathBuf::from("/home/me/.claude/settings.json")
    }

    fn denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "permission denied")
    }

    fn relay() -> Provider {
        Provider {
            id: "relay".into(),
            name: "Relay".into(),
            key: "sk-relay".into(),
            endpoints: BTreeMap::from([
                (Protocol::Anthropic, "https://relay.example".into()),
                (Protocol::OpenaiResponses, "https://relay.example/v1".into()),
            ]),
            models: vec![Model {
                id: "glm-4.6".into(),
                ..Model::default()
            }],
            ..Provider::default()
        }
    }

    /// core 真会给的：Codex 生效的 profile 选了别的路由。
    fn overridden() -> PatchError {
        let config = "profile = \"work\"\n\n[profiles.work]\nmodel_provider = \"azure\"\n";
        codex::patch(Some(&relay()), false)
            .unwrap()
            .apply(
                Path::new("/home/me/.codex/config.toml"),
                Some(config.as_bytes()),
            )
            .unwrap_err()
    }

    /// core 真会给的：Codex 的 `model_providers` 不是表。
    fn not_a_table() -> PatchError {
        codex::patch(Some(&relay()), false)
            .unwrap()
            .apply(
                Path::new("/home/me/.codex/config.toml"),
                Some(b"model_providers = 1\n"),
            )
            .unwrap_err()
    }

    /// core 真会给的：Claude Code 原始片段的 `env` 不是对象、`env` 里的值不是字符串。
    fn raw_shapes() -> Vec<AdaptError> {
        [
            json!({"env": "x"}),
            json!({"env": {"API_TIMEOUT_MS": 600000}}),
        ]
        .into_iter()
        .map(|raw| {
            let mut provider = relay();
            provider.overrides.insert(
                AgentKind::ClaudeCode,
                AgentOverride {
                    raw: raw.as_object().cloned(),
                    ..AgentOverride::default()
                },
            );
            claude::patch(Some(&provider), &[]).err().unwrap()
        })
        .collect()
    }

    fn engine() -> Vec<EngineError> {
        vec![
            EngineError::Io {
                path: path(),
                source: denied(),
            },
            EngineError::Patch(PatchError::Empty { path: path() }),
            EngineError::Conflict { path: path() },
        ]
    }

    fn patch() -> Vec<PatchError> {
        let check = |bytes: &[u8]| patch::check(Format::Json, &path(), bytes).unwrap_err();
        vec![
            // 解析器的原话、core 自己写的「不是 UTF-8」、顶层不是对象：都是 core 真会给的。
            check(b"{ nope"),
            check(b"{\"a\": \"\xff\"}"),
            check(b"[]"),
            not_a_table(),
            PatchError::Empty { path: path() },
            overridden(),
        ]
    }

    fn adapt() -> Vec<AdaptError> {
        let agent = AgentKind::ClaudeCode;
        let provider = || "Relay".to_string();
        let mut errors = vec![
            AdaptError::Incompatible {
                agent,
                provider: provider(),
                supported: &[Protocol::Anthropic, Protocol::OpenaiChat],
            },
            AdaptError::NoModels {
                agent,
                provider: provider(),
            },
            AdaptError::UnknownModel {
                provider: provider(),
                model: "glm-4.6".into(),
            },
            AdaptError::UnknownSlot {
                agent,
                slot: "main".into(),
            },
            AdaptError::MissingKey {
                agent,
                provider: provider(),
            },
            AdaptError::RawNotToml {
                agent: AgentKind::Codex,
                key: "x".into(),
            },
            AdaptError::RawSendsLogin {
                agent: AgentKind::Codex,
            },
            AdaptError::AnthropicOfficial {
                agent: AgentKind::GrokBuild,
            },
        ];
        errors.extend(raw_shapes());
        errors
    }

    fn apply() -> Vec<ApplyError> {
        vec![
            ApplyError::Adapt(AdaptError::NoModels {
                agent: AgentKind::Pi,
                provider: "Relay".into(),
            }),
            ApplyError::Engine(EngineError::Conflict { path: path() }),
            ApplyError::Parse(PatchError::Empty { path: path() }),
            ApplyError::WrongMode {
                agent: AgentKind::ClaudeCode,
            },
            ApplyError::WrongMode {
                agent: AgentKind::Opencode,
            },
        ]
    }

    fn provider() -> Vec<ProviderError> {
        let id = || "relay".to_string();
        vec![
            ProviderError::BadId {
                id: "Relay!".into(),
            },
            ProviderError::DuplicateId { id: id() },
            ProviderError::EmptyName { id: id() },
            ProviderError::NoEndpoint { id: id() },
            ProviderError::BadUrl {
                id: id(),
                protocol: Protocol::Anthropic,
                url: "relay.example".into(),
            },
            ProviderError::EmptyModelId { id: id() },
            ProviderError::DuplicateModel {
                id: id(),
                model: "glm-4.6".into(),
            },
            ProviderError::UnknownModel {
                id: id(),
                agent: AgentKind::Pi,
                model: "glm-4.6".into(),
            },
            ProviderError::UnknownDefaultModel {
                id: id(),
                model: "glm-4.6".into(),
            },
        ]
    }

    fn store() -> Vec<StoreError> {
        vec![
            StoreError::Io {
                path: path(),
                source: denied(),
            },
            StoreError::Parse {
                path: path(),
                line: 2,
                column: 5,
                message: "expected value".into(),
            },
            StoreError::Version {
                path: path(),
                found: 9,
            },
            StoreError::Invalid {
                path: path(),
                source: ProviderError::EmptyName { id: "relay".into() },
            },
            StoreError::Lock(EngineError::Conflict { path: path() }),
        ]
    }

    fn account() -> Vec<AccountError> {
        vec![
            AccountError::Engine(EngineError::Conflict { path: path() }),
            AccountError::Unsupported,
            AccountError::Missing,
            AccountError::Unrecognized,
            AccountError::NoProgram("codex"),
            AccountError::Login("Error: denied".into()),
            AccountError::LoginTimedOut,
        ]
    }

    fn net() -> Vec<NetError> {
        vec![
            NetError::BadProxy { url: "x".into() },
            NetError::BadUrl { url: "x".into() },
            NetError::Timeout,
            NetError::Unreachable(Unreachable::Dns),
            NetError::Unreachable(Unreachable::Refused),
            NetError::Unreachable(Unreachable::Tls),
            NetError::Unreachable(Unreachable::Proxy),
            NetError::Unreachable(Unreachable::Closed),
            NetError::Unreachable(Unreachable::Other("reset".into())),
            NetError::Status {
                status: 401,
                body: String::new(),
            },
            NetError::Status {
                status: 404,
                body: String::new(),
            },
            NetError::Status {
                status: 500,
                body: String::new(),
            },
            NetError::Parse {
                detail: "eof".into(),
            },
        ]
    }

    fn quota() -> Vec<QuotaError> {
        vec![
            QuotaError::NotLoggedIn,
            QuotaError::Expired,
            QuotaError::Auth,
            QuotaError::NoPlan,
            QuotaError::Net(NetError::Timeout),
            QuotaError::Parse("x".into()),
        ]
    }

    fn balance() -> Vec<BalanceError> {
        vec![
            BalanceError::NoKey,
            BalanceError::Auth,
            BalanceError::Net(NetError::Timeout),
            BalanceError::Parse("x".into()),
        ]
    }

    fn usage() -> Vec<UsageError> {
        vec![
            UsageError::Io {
                path: path(),
                source: denied(),
            },
            // 目录当成库打开：SQLite 的错误。
            Db::open(&std::env::temp_dir()).err().unwrap(),
        ]
    }

    fn skipped() -> Vec<Skipped> {
        vec![Skipped::NotFound, Skipped::Failed, Skipped::NoTemplate]
    }

    /// 样本覆盖了每个变体。编号的 `match` 不写 `_`：core 加了变体这里编不过，补上编号和样本才过。
    fn covers<E: std::fmt::Debug>(samples: &[E], kind: fn(&E) -> usize, count: usize) {
        let seen: BTreeSet<usize> = samples.iter().map(kind).collect();
        assert_eq!(seen, (0..count).collect(), "{samples:?}");
    }

    #[test]
    fn every_variant_has_a_sample() {
        covers(
            &engine(),
            |e| match e {
                EngineError::Io { .. } => 0,
                EngineError::Patch(_) => 1,
                EngineError::Conflict { .. } => 2,
            },
            3,
        );
        covers(
            &patch(),
            |e| match e {
                PatchError::Parse { .. } => 0,
                PatchError::Shape { .. } => 1,
                PatchError::Empty { .. } => 2,
                PatchError::Overridden { .. } => 3,
            },
            4,
        );
        covers(
            &adapt(),
            |e| match e {
                AdaptError::Incompatible { .. } => 0,
                AdaptError::NoModels { .. } => 1,
                AdaptError::UnknownModel { .. } => 2,
                AdaptError::UnknownSlot { .. } => 3,
                AdaptError::MissingKey { .. } => 4,
                AdaptError::RawShape { .. } => 5,
                AdaptError::RawNotToml { .. } => 6,
                AdaptError::RawSendsLogin { .. } => 7,
                AdaptError::AnthropicOfficial { .. } => 8,
            },
            9,
        );
        covers(
            &apply(),
            |e| match e {
                ApplyError::Adapt(_) => 0,
                ApplyError::Engine(_) => 1,
                ApplyError::Parse(_) => 2,
                ApplyError::WrongMode { .. } => 3,
            },
            4,
        );
        covers(
            &provider(),
            |e| match e {
                ProviderError::BadId { .. } => 0,
                ProviderError::DuplicateId { .. } => 1,
                ProviderError::EmptyName { .. } => 2,
                ProviderError::NoEndpoint { .. } => 3,
                ProviderError::BadUrl { .. } => 4,
                ProviderError::EmptyModelId { .. } => 5,
                ProviderError::DuplicateModel { .. } => 6,
                ProviderError::UnknownModel { .. } => 7,
                ProviderError::UnknownDefaultModel { .. } => 8,
            },
            9,
        );
        covers(
            &store(),
            |e| match e {
                StoreError::Io { .. } => 0,
                StoreError::Parse { .. } => 1,
                StoreError::Version { .. } => 2,
                StoreError::Invalid { .. } => 3,
                StoreError::Lock(_) => 4,
            },
            5,
        );
        covers(
            &account(),
            |e| match e {
                AccountError::Engine(_) => 0,
                AccountError::Unsupported => 1,
                AccountError::Missing => 2,
                AccountError::Unrecognized => 3,
                AccountError::NoProgram(_) => 4,
                AccountError::Login(_) => 5,
                AccountError::LoginTimedOut => 6,
            },
            7,
        );
        covers(
            &net(),
            |e| match e {
                NetError::BadProxy { .. } => 0,
                NetError::BadUrl { .. } => 1,
                NetError::Timeout => 2,
                NetError::Unreachable(Unreachable::Dns) => 3,
                NetError::Unreachable(Unreachable::Refused) => 4,
                NetError::Unreachable(Unreachable::Tls) => 5,
                NetError::Unreachable(Unreachable::Proxy) => 6,
                NetError::Unreachable(Unreachable::Closed) => 7,
                NetError::Unreachable(Unreachable::Other(_)) => 8,
                NetError::Status { .. } => 9,
                NetError::Parse { .. } => 10,
            },
            11,
        );
        covers(
            &quota(),
            |e| match e {
                QuotaError::NotLoggedIn => 0,
                QuotaError::Expired => 1,
                QuotaError::Auth => 2,
                QuotaError::NoPlan => 3,
                QuotaError::Net(_) => 4,
                QuotaError::Parse(_) => 5,
            },
            6,
        );
        covers(
            &balance(),
            |e| match e {
                BalanceError::NoKey => 0,
                BalanceError::Auth => 1,
                BalanceError::Net(_) => 2,
                BalanceError::Parse(_) => 3,
            },
            4,
        );
        covers(
            &usage(),
            |e| match e {
                UsageError::Db { .. } => 0,
                UsageError::Io { .. } => 1,
            },
            2,
        );
        covers(
            &skipped(),
            |e| match e {
                Skipped::NotFound => 0,
                Skipped::Failed => 1,
                Skipped::NoTemplate => 2,
            },
            3,
        );
    }

    fn chinese(text: &str) -> bool {
        text.chars().any(|ch| {
            ('\u{3000}'..='\u{303f}').contains(&ch)
                || ('\u{4e00}'..='\u{9fff}').contains(&ch)
                || ('\u{ff00}'..='\u{ffef}').contains(&ch)
        })
    }

    /// 英文界面整句是英文（B6）；中文的没有漏翻成键名。
    fn translated<E: Describe + std::fmt::Debug>(errors: &[E]) {
        for err in errors {
            let (en, zh) = (err.describe_in(EN), err.describe_in(ZH));
            assert!(!chinese(&en), "Chinese in English: {en} ({err:?})");
            assert!(chinese(&zh), "no Chinese in Chinese: {zh} ({err:?})");
        }
    }

    /// 只查英文里没有中文（有几句中英文一样，比如「HTTP 500」、路径加系统的原话）。
    fn english<E: Describe + std::fmt::Debug>(errors: &[E]) {
        for err in errors {
            let en = err.describe_in(EN);
            assert!(!chinese(&en), "Chinese in English: {en} ({err:?})");
        }
    }

    /// 中文和 core 的原话一样：中文界面不变。
    fn unchanged<E: Describe + std::fmt::Display>(errors: &[E]) {
        for err in errors {
            assert_eq!(err.describe_in(ZH), err.to_string());
        }
    }

    #[test]
    fn every_error_is_english_in_english() {
        translated(&engine());
        translated(&patch());
        translated(&adapt());
        translated(&apply());
        translated(&provider());
        translated(&store());
        translated(&account());
        translated(&skipped());
        translated(&quota());
        translated(&balance());
        english(&net());
        english(&usage());
    }

    #[test]
    fn every_shorter_form_is_english_in_english() {
        let providers = provider();
        translated(&providers.iter().map(InEditor).collect::<Vec<_>>());
        let patches = patch();
        translated(&patches.iter().map(InRaw).collect::<Vec<_>>());
        let engines = engine();
        translated(&engines.iter().map(InRawWrite).collect::<Vec<_>>());
        let stores = store();
        translated(&stores.iter().map(Importing).collect::<Vec<_>>());
        let nets = net();
        english(&nets.iter().map(Listing).collect::<Vec<_>>());
    }

    #[test]
    fn chinese_is_what_core_says() {
        unchanged(&engine());
        unchanged(&patch());
        unchanged(&adapt());
        unchanged(&apply());
        unchanged(&provider());
        unchanged(&store());
        // 登录、联网、额度、余额的中文原来就是 app 自己的说法（比如「登录超时」，core 是「登录超时，已停止」），不查。
        unchanged(&account()[..6]);
        unchanged(&usage());
    }

    /// 解析错误：英文不重复位置（解析器原话末尾的「at line 1 column 3」）；「原始文件」编辑框里两种语言都不重复。
    #[test]
    fn a_parse_error_says_where_once() {
        let err = patch::check(Format::Json, &path(), b"{ nope").unwrap_err();
        assert_eq!(
            err.describe_in(EN),
            "Can't parse /home/me/.claude/settings.json (line 1, column 3): key must be a string"
        );
        assert_eq!(
            InRaw(&err).describe_in(ZH),
            "第 1 行第 3 列解析不了：key must be a string"
        );
    }
}
