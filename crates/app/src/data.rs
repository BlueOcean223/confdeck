//! 原型阶段的假数据。以后由 confdeck-core 读写真实的配置文件；结构先照 core 的来：
//! agent 分单选 / 多选（`Mode`），供应商记提供哪几种接口（`Protocol`）。

use confdeck_core::agent::Mode;
use confdeck_core::provider::Protocol;
use gpui_kit::*;
use rust_i18n::t;

/// 图标资源。单色图标按文字颜色着色，彩色图标按原色绘制。
#[derive(Clone, Copy)]
pub struct Logo {
    pub path: &'static str,
    pub colored: bool,
}

const fn mono(path: &'static str) -> Logo {
    Logo {
        path,
        colored: false,
    }
}

const fn color(path: &'static str) -> Logo {
    Logo {
        path,
        colored: true,
    }
}

pub struct Provider {
    pub name: &'static str,
    pub logo: Logo,
    pub host: &'static str,
    pub key: &'static str,
    /// 提供哪几种接口。agent 要的这里都没有，就是「接口对不上」：选择器里置灰并写明缺哪种。
    pub protocols: &'static [Protocol],
}

/// agent 用哪家：单选 agent 一家（`None` 是「默认」，agent 自带的登录），多选 agent 勾了哪几家。
pub enum Selection {
    Single(Option<usize>),
    Multi(Vec<usize>),
}

pub struct Agent {
    pub name: &'static str,
    pub logo: Logo,
    pub config: &'static str,
    pub installed: bool,
    pub mode: Mode,
    /// 能说的协议，按偏好排序：供应商提供好几种时用第一种（decisions「供应商和 agent 的关系」）。
    pub protocols: &'static [Protocol],
    pub selection: Selection,
    /// 在 confdeck 里给它选过供应商（core 里是 `state.json` 的 `selections`）。
    /// 菜单栏面板只列这样的单选 agent（decisions「没有『接管』」）。
    pub chosen: bool,
}

pub struct Store {
    pub providers: Vec<Provider>,
    pub agents: Vec<Agent>,
}

impl Store {
    pub fn demo() -> Self {
        use Protocol::*;
        let p = |name, logo, host, key, protocols| Provider {
            name,
            logo,
            host,
            key,
            protocols,
        };
        let single = |name, logo, config, installed, protocols, provider: Option<usize>| Agent {
            name,
            logo,
            config,
            installed,
            mode: Mode::Switch,
            protocols,
            chosen: installed && provider.is_some(),
            selection: Selection::Single(provider),
        };
        let multi = |name, logo, config, installed, protocols, enabled| Agent {
            name,
            logo,
            config,
            installed,
            mode: Mode::Coexist,
            protocols,
            chosen: false,
            selection: Selection::Multi(enabled),
        };
        Self {
            providers: vec![
                p(
                    "Anthropic",
                    mono("icons/brand/anthropic.svg"),
                    "api.anthropic.com",
                    "sk-a…0000",
                    &[Anthropic],
                ),
                p(
                    "DeepSeek",
                    color("icons/brand/deepseek-color.svg"),
                    "api.deepseek.com",
                    "sk-d…0000",
                    &[OpenaiChat, Anthropic],
                ),
                p(
                    "Kimi",
                    mono("icons/brand/kimi.svg"),
                    "api.moonshot.cn",
                    "sk-k…0000",
                    &[OpenaiChat, Anthropic],
                ),
                p(
                    "智谱 GLM",
                    color("icons/brand/zhipu-color.svg"),
                    "open.bigmodel.cn",
                    "sk-z…0000",
                    &[OpenaiChat, Anthropic, OpenaiResponses],
                ),
                p(
                    "Qwen",
                    color("icons/brand/qwen-color.svg"),
                    "dashscope.aliyuncs.com",
                    "sk-q…0000",
                    &[OpenaiChat, Anthropic, OpenaiResponses],
                ),
                p(
                    "MiniMax",
                    color("icons/brand/minimax-color.svg"),
                    "api.minimaxi.com",
                    "sk-m…0000",
                    &[Anthropic, OpenaiChat],
                ),
                p(
                    "OpenRouter",
                    mono("icons/brand/openrouter.svg"),
                    "openrouter.ai",
                    "sk-o…0000",
                    &[OpenaiChat, OpenaiResponses],
                ),
            ],
            // 协议偏好：前五个照 core 的 `AgentKind::protocols`；后补的四个照 fork，阶段 7 加适配器时再核对。
            agents: vec![
                single(
                    "Claude Code",
                    color("icons/brand/claudecode-color.svg"),
                    "~/.claude/settings.json",
                    true,
                    &[Anthropic],
                    Some(0),
                ),
                single(
                    "Codex",
                    color("icons/brand/codex-color.svg"),
                    "~/.codex/config.toml",
                    true,
                    &[OpenaiResponses],
                    Some(1),
                ),
                single(
                    "Gemini CLI",
                    color("icons/brand/geminicli-color.svg"),
                    "~/.gemini/.env",
                    true,
                    &[Gemini],
                    None,
                ),
                multi(
                    "OpenCode",
                    mono("icons/brand/opencode.svg"),
                    "~/.config/opencode/opencode.json",
                    true,
                    &[OpenaiChat, Anthropic, OpenaiResponses, Gemini],
                    vec![4, 1],
                ),
                multi(
                    "Pi",
                    mono("icons/brand/pi.svg"),
                    "~/.pi/agent/models.json",
                    true,
                    &[OpenaiChat, Anthropic, OpenaiResponses, Gemini],
                    vec![6, 1, 2],
                ),
                single(
                    "Grok Build",
                    mono("icons/brand/grok.svg"),
                    "~/.grok/config.toml",
                    false,
                    &[OpenaiResponses, OpenaiChat, Anthropic],
                    None,
                ),
                multi(
                    "OpenClaw",
                    color("icons/brand/openclaw-color.svg"),
                    "~/.openclaw/openclaw.json",
                    false,
                    &[OpenaiChat, Anthropic],
                    vec![],
                ),
                multi(
                    "Hermes",
                    color("icons/brand/hermes.png"),
                    "~/.hermes/config.yaml",
                    false,
                    &[OpenaiChat],
                    vec![],
                ),
            ],
        }
    }

    /// 单选 agent 换一家（`None` 是「默认」）。
    pub fn set_provider(&mut self, agent: usize, provider: Option<usize>, cx: &mut Context<Self>) {
        let agent = &mut self.agents[agent];
        agent.selection = Selection::Single(provider);
        agent.chosen = true;
        cx.notify();
    }

    /// 多选 agent 勾上或取消一家。
    pub fn toggle_provider(&mut self, agent: usize, provider: usize, cx: &mut Context<Self>) {
        if let Selection::Multi(enabled) = &mut self.agents[agent].selection {
            match enabled.iter().position(|&p| p == provider) {
                Some(at) => {
                    enabled.remove(at);
                }
                None => enabled.push(provider),
            }
            cx.notify();
        }
    }

    pub fn is_enabled(&self, agent: usize, provider: usize) -> bool {
        match &self.agents[agent].selection {
            Selection::Single(current) => *current == Some(provider),
            Selection::Multi(enabled) => enabled.contains(&provider),
        }
    }

    /// 这个 agent 连这家会用哪种协议：agent 偏好顺序里这家提供的第一种；一种都没有就是接口对不上。
    pub fn protocol_for(&self, agent: usize, provider: usize) -> Option<Protocol> {
        let provider = &self.providers[provider];
        self.agents[agent]
            .protocols
            .iter()
            .copied()
            .find(|protocol| provider.protocols.contains(protocol))
    }

    /// 胶囊里画什么：要叠的图标（单选是一个）和文字。
    pub fn pill(&self, agent: usize) -> (Vec<Logo>, SharedString) {
        let agent = &self.agents[agent];
        match &agent.selection {
            Selection::Single(Some(p)) => (
                vec![self.providers[*p].logo],
                self.providers[*p].name.into(),
            ),
            Selection::Single(None) => (vec![agent.logo], t!("common.default").into()),
            Selection::Multi(enabled) if enabled.is_empty() => {
                (vec![agent.logo], t!("multi.none").into())
            }
            Selection::Multi(enabled) => {
                let logos = enabled.iter().map(|&p| self.providers[p].logo).collect();
                let label = if enabled.len() == 1 {
                    t!("multi.count_one")
                } else {
                    t!("multi.count_other", count = enabled.len())
                };
                (logos, label.into())
            }
        }
    }

    /// 某个 provider 正被哪些 agent 使用。
    pub fn users_of(&self, provider: usize) -> Vec<&'static str> {
        (0..self.agents.len())
            .filter(|&a| self.is_enabled(a, provider))
            .map(|a| self.agents[a].name)
            .collect()
    }
}
