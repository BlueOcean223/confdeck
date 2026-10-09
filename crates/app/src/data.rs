//! 原型阶段的假数据。以后由 confdeck-core 读写真实的配置文件。

use gpui_kit::*;

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
}

pub struct Agent {
    pub name: &'static str,
    pub logo: Logo,
    pub config: &'static str,
    pub installed: bool,
    /// 是否由 confdeck 接管这个 agent 的配置文件。不接管时一个字节都不改。
    pub managed: bool,
    /// `None` 表示用 agent 自带的登录（官方账号）。
    pub provider: Option<usize>,
}

pub struct Store {
    pub providers: Vec<Provider>,
    pub agents: Vec<Agent>,
}

impl Store {
    pub fn demo() -> Self {
        let p = |name, logo, host, key| Provider {
            name,
            logo,
            host,
            key,
        };
        let a = |name, logo, config, installed, provider| Agent {
            name,
            logo,
            config,
            installed,
            managed: installed,
            provider,
        };
        Self {
            providers: vec![
                p("Anthropic", mono("icons/brand/anthropic.svg"), "api.anthropic.com", "sk-a…0000"),
                p("DeepSeek", color("icons/brand/deepseek-color.svg"), "api.deepseek.com", "sk-d…0000"),
                p("Kimi", mono("icons/brand/kimi.svg"), "api.moonshot.cn", "sk-k…0000"),
                p("智谱 GLM", color("icons/brand/zhipu-color.svg"), "open.bigmodel.cn", "sk-z…0000"),
                p("Qwen", color("icons/brand/qwen-color.svg"), "dashscope.aliyuncs.com", "sk-q…0000"),
                p("MiniMax", color("icons/brand/minimax-color.svg"), "api.minimaxi.com", "sk-m…0000"),
                p("OpenRouter", mono("icons/brand/openrouter.svg"), "openrouter.ai", "sk-o…0000"),
            ],
            agents: vec![
                a("Claude Code", color("icons/brand/claudecode-color.svg"), "~/.claude/settings.json", true, Some(0)),
                a("Codex", color("icons/brand/codex-color.svg"), "~/.codex/config.toml", true, Some(1)),
                a("Gemini CLI", color("icons/brand/geminicli-color.svg"), "~/.gemini/.env", true, Some(3)),
                a("OpenCode", mono("icons/brand/opencode.svg"), "~/.config/opencode/opencode.json", true, Some(4)),
                a("Pi", mono("icons/brand/pi.svg"), "~/.pi/agent/models.json", true, Some(6)),
                a("Grok Build", mono("icons/brand/grok.svg"), "~/.grok/config.toml", false, None),
                a("Hermes", mono("icons/brand/hermes.svg"), "~/.hermes/config.yaml", false, None),
                a("MiniMax Code", color("icons/brand/minimax-color.svg"), "~/.mcode/config.json", false, None),
            ],
        }
    }

    pub fn set_provider(&mut self, agent: usize, provider: Option<usize>, cx: &mut Context<Self>) {
        self.agents[agent].provider = provider;
        cx.notify();
    }

    pub fn toggle_managed(&mut self, agent: usize, cx: &mut Context<Self>) {
        self.agents[agent].managed = !self.agents[agent].managed;
        cx.notify();
    }

    pub fn provider_label(&self, agent: usize) -> (Logo, &'static str) {
        let agent = &self.agents[agent];
        match agent.provider {
            Some(p) => (self.providers[p].logo, self.providers[p].name),
            None => (agent.logo, "默认"),
        }
    }

    /// 某个 provider 正被哪些 agent 使用。
    pub fn users_of(&self, provider: usize) -> Vec<&'static str> {
        self.agents
            .iter()
            .filter(|a| a.provider == Some(provider))
            .map(|a| a.name)
            .collect()
    }
}
