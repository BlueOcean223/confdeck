//! 会话：各 agent 的本地会话列表、看一个会话说了什么、复制恢复命令（会话页）。
//!
//! 列表不另外扫文件：用量扫会话记录时顺带记下每个会话的概况（[`crate::usage::Db::sessions`]）。
//! 看对话时才读那一个文件，只取提问、回答的文字和调用了什么工具（工具的输出、思考过程不要）。
//!
//! | agent | 恢复命令（先 `cd` 到会话的目录） |
//! | --- | --- |
//! | Claude Code | `claude --resume <id>` |
//! | Codex | `codex resume <id>` |
//! | Pi | `pi --session <文件>` |
//! | Grok Build | `grok --resume <id>` |
//! | OpenCode | `opencode -s <id>`（还没接上会话列表） |

use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use serde_json::Value;

use crate::agent::AgentKind;

/// 一个会话的概况。
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub agent: AgentKind,
    pub id: String,
    /// 会话记录的文件（Grok Build 是会话目录里的 `updates.jsonl`）。
    pub path: PathBuf,
    /// 在哪个目录里开的。
    pub cwd: Option<String>,
    /// 第一句提问（压成一行，最多 200 个字）。
    pub prompt: Option<String>,
    /// 用户起的名字。
    pub name: Option<String>,
    /// agent 自己起的标题。
    pub title: Option<String>,
    /// 开始时间（毫秒）。
    pub started: Option<i64>,
    /// 最后活动时间（毫秒，文件的修改时间）。
    pub active: i64,
}

impl Session {
    /// 列表里写的名字：用户起的 > agent 起的标题 > 第一句提问 > 目录名 > 会话 ID。
    pub fn label(&self) -> String {
        let folder = self.cwd.as_deref().and_then(|cwd| {
            std::path::Path::new(cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .or_else(|| self.prompt.clone())
            .or(folder)
            .unwrap_or_else(|| self.id.clone())
    }

    /// 在终端里接着这个会话的命令：先 `cd` 到会话的目录。
    pub fn resume_command(&self) -> String {
        let run = match self.agent {
            AgentKind::ClaudeCode => format!("claude --resume {}", quote(&self.id)),
            AgentKind::Codex => format!("codex resume {}", quote(&self.id)),
            AgentKind::Pi => format!("pi --session {}", quote(&self.path.to_string_lossy())),
            AgentKind::GrokBuild => format!("grok --resume {}", quote(&self.id)),
            AgentKind::Opencode => format!("opencode -s {}", quote(&self.id)),
        };
        match self.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => format!("cd {} && {run}", quote(cwd)),
            None => run,
        }
    }
}

/// 给 shell 用的引号：只有字母、数字和 `_-./~:@%+=,` 的原样，别的用单引号包起来。
fn quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:@%+=,".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// 对话里的一条。
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    /// 提问、回答是原文（太长的截断）；工具是「名字 · 主要的参数」。
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// 一条消息最多留多少个字。
const MAX_TEXT: usize = 4000;
/// 最多留最后多少条。
const MAX_MESSAGES: usize = 400;

/// 读出一个会话的对话：提问、回答的文字和调用了什么工具。太长的只留最后 [`MAX_MESSAGES`] 条，每条最多 [`MAX_TEXT`] 个字。
/// 文件可能很大（几十 MB），放后台线程里读。
pub fn messages(session: &Session) -> std::io::Result<Vec<Message>> {
    let file = std::fs::File::open(&session.path)?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match session.agent {
            AgentKind::ClaudeCode => claude(&value, &mut out),
            AgentKind::Codex => codex(&value, &mut out),
            AgentKind::Pi => pi(&value, &mut out),
            AgentKind::GrokBuild => grok(&value, &mut out),
            AgentKind::Opencode => {}
        }
    }
    if out.len() > MAX_MESSAGES {
        out.drain(..out.len() - MAX_MESSAGES);
    }
    Ok(out)
}

fn push(out: &mut Vec<Message>, role: Role, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let mut short: String = text.chars().take(MAX_TEXT).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    out.push(Message { role, text: short });
}

/// 工具调用写成「名字 · 主要的参数」：命令、文件、搜的东西，取第一个有的，压成一行、最多 120 个字。
fn push_tool(out: &mut Vec<Message>, name: &str, input: &Value) {
    let input = match input {
        // Codex 的参数是一段 JSON 字符串。
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::String(text.clone())),
        other => other.clone(),
    };
    let main = [
        "command",
        "cmd",
        "file_path",
        "path",
        "target_file",
        "pattern",
        "query",
        "url",
        "description",
    ]
    .iter()
    .find_map(|key| match &input[*key] {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    })
    .or_else(|| input.as_str().map(str::to_string));
    let mut text = name.to_string();
    if let Some(main) = main {
        let main: String = main.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut short: String = main.chars().take(120).collect();
        if short.len() < main.len() {
            short.push('…');
        }
        text.push_str(" · ");
        text.push_str(&short);
    }
    out.push(Message {
        role: Role::Tool,
        text,
    });
}

/// Claude Code：`type` 是 `user` / `assistant` 的行，`message.content` 是字符串或内容块
/// （`text`、`tool_use`；`tool_result`、`thinking` 不要）。`isMeta` 和 `<` 开头的是 Claude Code 自己插的。
fn claude(value: &Value, out: &mut Vec<Message>) {
    let role = match value["type"].as_str() {
        Some("user") if value["isMeta"] != true => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return,
    };
    let content = &value["message"]["content"];
    if let Some(text) = content.as_str() {
        if !text.trim_start().starts_with('<') {
            push(out, role, text);
        }
        return;
    }
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().unwrap_or_default();
                if !(role == Role::User && text.trim_start().starts_with('<')) {
                    push(out, role, text);
                }
            }
            Some("tool_use") => push_tool(
                out,
                block["name"].as_str().unwrap_or("tool"),
                &block["input"],
            ),
            _ => {}
        }
    }
}

/// Codex：`response_item` 里 `type: message` 的（用户的 `input_text`，去掉 `<` 开头和 `# AGENTS.md` 开头的；
/// 回答的 `output_text`），`function_call` / `custom_tool_call` 是工具。
fn codex(value: &Value, out: &mut Vec<Message>) {
    if value["type"] != "response_item" {
        return;
    }
    let payload = &value["payload"];
    match payload["type"].as_str() {
        Some("message") => {
            let role = match payload["role"].as_str() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => return,
            };
            for part in payload["content"].as_array().into_iter().flatten() {
                let text = part["text"].as_str().unwrap_or_default();
                let injected = text.trim_start().starts_with('<')
                    || text.trim_start().starts_with("# AGENTS.md");
                if !(role == Role::User && injected) {
                    push(out, role, text);
                }
            }
        }
        Some("function_call") => push_tool(
            out,
            payload["name"].as_str().unwrap_or("tool"),
            &payload["arguments"],
        ),
        Some("custom_tool_call") => push_tool(
            out,
            payload["name"].as_str().unwrap_or("tool"),
            &payload["input"],
        ),
        _ => {}
    }
}

/// Pi：`type: message`，`message.role` 是 `user` / `assistant`，`content` 是字符串或内容块（`text`、`toolCall`）。
fn pi(value: &Value, out: &mut Vec<Message>) {
    if value["type"] != "message" {
        return;
    }
    let message = &value["message"];
    let role = match message["role"].as_str() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return,
    };
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        push(out, role, text);
        return;
    }
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => push(out, role, block["text"].as_str().unwrap_or_default()),
            Some("toolCall") => push_tool(
                out,
                block["name"].as_str().unwrap_or("tool"),
                &block["arguments"],
            ),
            _ => {}
        }
    }
}

/// Grok Build：`params.update.sessionUpdate` 是 `user_message_chunk` / `agent_message_chunk` 的一块一块拼起来，
/// `tool_call` 用它的 `title`。
fn grok(value: &Value, out: &mut Vec<Message>) {
    let update = &value["params"]["update"];
    let role = match update["sessionUpdate"].as_str() {
        Some("user_message_chunk") => Role::User,
        Some("agent_message_chunk") => Role::Assistant,
        Some("tool_call") => {
            let title = update["title"].as_str().unwrap_or("tool");
            out.push(Message {
                role: Role::Tool,
                text: title.chars().take(160).collect(),
            });
            return;
        }
        _ => return,
    };
    let text = update["content"]["text"].as_str().unwrap_or_default();
    match out.last_mut() {
        Some(last) if last.role == role => {
            if last.text.chars().count() < MAX_TEXT {
                last.text.push_str(text);
            }
        }
        _ if !text.trim().is_empty() => out.push(Message {
            role,
            text: text.trim_start().to_string(),
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{Message, Role, Session, claude, codex, grok, pi, quote};
    use crate::agent::AgentKind;

    fn session(agent: AgentKind, cwd: Option<&str>) -> Session {
        Session {
            agent,
            id: "abc-123".into(),
            path: PathBuf::from("/h/.pi/agent/sessions/x/2026_abc.jsonl"),
            cwd: cwd.map(str::to_string),
            prompt: Some("fix the build".into()),
            name: None,
            title: None,
            started: None,
            active: 0,
        }
    }

    #[test]
    fn resume_commands_cd_into_the_folder() {
        assert_eq!(
            session(AgentKind::ClaudeCode, Some("/Users/me/code/app")).resume_command(),
            "cd /Users/me/code/app && claude --resume abc-123"
        );
        assert_eq!(
            session(AgentKind::Codex, Some("/Users/me/My Project")).resume_command(),
            "cd '/Users/me/My Project' && codex resume abc-123"
        );
        assert_eq!(
            session(AgentKind::Pi, None).resume_command(),
            "pi --session /h/.pi/agent/sessions/x/2026_abc.jsonl"
        );
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn labels_fall_back_in_order() {
        let mut s = session(AgentKind::ClaudeCode, Some("/w/app"));
        assert_eq!(s.label(), "fix the build");
        s.title = Some("Build fix".into());
        assert_eq!(s.label(), "Build fix");
        s.name = Some("mine".into());
        assert_eq!(s.label(), "mine");
        let mut s = session(AgentKind::ClaudeCode, Some("/w/app"));
        s.prompt = None;
        assert_eq!(s.label(), "app");
    }

    #[test]
    fn each_agent_reads_text_and_tools() {
        let mut out = Vec::new();
        claude(
            &json!({"type": "user", "message": {"content": "<command-name>/clear</command-name>"}}),
            &mut out,
        );
        claude(
            &json!({"type": "user", "message": {"content": "hi"}}),
            &mut out,
        );
        claude(
            &json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo   test"}}]}}),
            &mut out,
        );
        claude(
            &json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
            &mut out,
        );
        assert_eq!(
            out,
            [
                Message {
                    role: Role::User,
                    text: "hi".into()
                },
                Message {
                    role: Role::Assistant,
                    text: "hello".into()
                },
                Message {
                    role: Role::Tool,
                    text: "Bash · cargo test".into()
                },
            ]
        );

        let mut out = Vec::new();
        codex(
            &json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "<environment_context>x</environment_context>"},
            {"type": "input_text", "text": "add a test"}]}}),
            &mut out,
        );
        codex(
            &json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": "{\"command\":[\"ls\",\"-la\"]}"}}),
            &mut out,
        );
        assert_eq!(out[0].text, "add a test");
        assert_eq!(out[1].text, "shell · ls -la");

        let mut out = Vec::new();
        pi(
            &json!({"type": "message", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "done"}, {"type": "toolCall", "name": "read", "arguments": {"path": "a.rs"}}]}}),
            &mut out,
        );
        assert_eq!(out[1].text, "read · a.rs");

        let mut out = Vec::new();
        for (kind, text) in [
            ("user_message_chunk", "fix "),
            ("user_message_chunk", "it"),
            ("agent_message_chunk", "ok"),
        ] {
            grok(
                &json!({"params": {"update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}}),
                &mut out,
            );
        }
        assert_eq!(out[0].text, "fix it");
        assert_eq!(out[1].role, Role::Assistant);
    }
}
