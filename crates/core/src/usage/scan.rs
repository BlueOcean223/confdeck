//! 增量扫描：每个会话文件记着读到哪，只读后面新加的整行；文件变短了（被重写）从头读，靠请求的键去重。

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::db::FileRow;
use super::{Db, Request, SessionPatch, UsageError, claude, codex, grok, opencode, pi};
use crate::agent::AgentKind;

/// 一次扫描的结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// 找到的会话文件。
    pub files: usize,
    /// 有新内容、读了的。
    pub read: usize,
    /// 新加的请求。
    pub added: usize,
    /// 读不了的文件和原因；不影响别的文件。
    pub errors: Vec<String>,
}

/// 一个文件的解析器：一行一行喂进去，最后交出请求、要接着用的状态和会话概况。
pub(super) trait Parser {
    /// `offset` 是这一行在文件里的起点。
    fn line(&mut self, offset: u64, line: &str);
    fn finish(self: Box<Self>) -> Parsed;
}

#[derive(Default)]
pub(super) struct Parsed {
    pub requests: Vec<Request>,
    /// 下次接着读时交回给解析器的状态（各家自己定格式，空串是没有）。
    pub state: String,
    /// 会话概况的更新；不是会话（子代理）的是 `None`。
    pub session: Option<SessionPatch>,
}

struct Source {
    agent: AgentKind,
    files: fn(&Path) -> Vec<PathBuf>,
    open: fn(&Path, &str) -> Box<dyn Parser>,
}

const SOURCES: [Source; 4] = [
    Source {
        agent: AgentKind::ClaudeCode,
        files: claude::files,
        open: claude::open,
    },
    Source {
        agent: AgentKind::Codex,
        files: codex::files,
        open: codex::open,
    },
    Source {
        agent: AgentKind::Pi,
        files: pi::files,
        open: pi::open,
    },
    Source {
        agent: AgentKind::GrokBuild,
        files: grok::files,
        open: grok::open,
    },
];

/// 扫一遍各 agent 的会话记录（`home` 是家目录）。第一次要把历史全读一遍（本机 5 GB、1600 个文件，release 约 6 秒），
/// 之后只读新加的（几十毫秒）。放后台线程里跑。
pub fn scan(home: &Path, db: &mut Db) -> Report {
    let mut report = Report::default();
    for source in &SOURCES {
        for path in (source.files)(home) {
            report.files += 1;
            match scan_file(db, source, &path) {
                Ok(Some(added)) => {
                    report.read += 1;
                    report.added += added;
                }
                Ok(None) => {}
                Err(err) => {
                    report
                        .errors
                        .push(format!("{} {}: {err}", source.agent.id(), path.display()))
                }
            }
        }
    }
    // OpenCode 是一个 SQLite 库，不是一行一行的文件。
    opencode::scan(home, db, &mut report);
    report
}

/// 没变的文件返回 `None`；读了返回新加了几条。
fn scan_file(db: &mut Db, source: &Source, path: &Path) -> Result<Option<usize>, UsageError> {
    let io = |source| UsageError::Io {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io)?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos() as i64);
    let key = path.to_string_lossy();
    let previous = db.file(&key)?;
    if previous
        .as_ref()
        .is_some_and(|row| row.size == size && row.mtime == mtime)
    {
        return Ok(None);
    }
    let (offset, state) = match previous {
        Some(row) if size >= row.offset => (row.offset, row.state),
        _ => (0, String::new()),
    };
    let mut parser = (source.open)(path, &state);
    let end = read_lines(path, offset, |at, line| parser.line(at, line)).map_err(io)?;
    let parsed = parser.finish();
    let row = FileRow {
        size,
        mtime,
        offset: end,
        state: parsed.state,
    };
    // 最后活动时间用文件的修改时间，不用一行一行去解析时间。
    let session = parsed
        .session
        .as_ref()
        .map(|patch| (source.agent, patch, mtime / 1_000_000));
    db.commit(&key, &row, &parsed.requests, session).map(Some)
}

/// 从 `offset` 起逐个读整行（不带换行符），交给 `line`；最后一行还没写完（没有换行符）的不读。
/// 返回读完的最后一个整行之后的位置。不是 UTF-8 的行跳过。
fn read_lines(path: &Path, offset: u64, mut line: impl FnMut(u64, &str)) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut buf = Vec::new();
    let mut at = offset;
    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 || buf.last() != Some(&b'\n') {
            return Ok(at);
        }
        if let Ok(text) = std::str::from_utf8(&buf[..buf.len() - 1]) {
            line(at, text.trim_end_matches('\r'));
        }
        at += read as u64;
    }
}

/// `dir` 下（最多 `depth` 层）名字满足 `wanted` 的文件。目录不在就是空的。
pub(super) fn walk(dir: &Path, depth: usize, wanted: fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth > 1 {
                found.extend(walk(&path, depth - 1, wanted));
            }
        } else if kind.is_file() && wanted(&path) {
            found.push(path);
        }
    }
    found.sort();
    found
}

pub(super) fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{read_lines, scan};
    use crate::agent::AgentKind;
    use crate::usage::Db;

    #[test]
    fn only_finished_lines_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, "one\ntwo\nthr").unwrap();
        let mut lines = Vec::new();
        let end = read_lines(&path, 0, |at, line| lines.push((at, line.to_string()))).unwrap();
        assert_eq!(lines, vec![(0, "one".to_string()), (4, "two".to_string())]);
        assert_eq!(end, 8);

        // 接着写完那一行：从上次的位置读起。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"ee\n").unwrap();
        let mut lines = Vec::new();
        let end = read_lines(&path, end, |at, line| lines.push((at, line.to_string()))).unwrap();
        assert_eq!(lines, vec![(8, "three".to_string())]);
        assert_eq!(end, 14);
    }

    fn write(home: &std::path::Path, relative: &str, lines: &[&str]) {
        let path = home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn sessions_are_noted_while_scanning() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        write(
            home,
            ".claude/projects/-w-app/c1.jsonl",
            &[
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"},"cwd":"/w/app","timestamp":"2026-10-12T01:00:00.000Z","sessionId":"c1"}"#,
                r#"{"type":"user","message":{"role":"user","content":"  fix   the build\nplease "},"cwd":"/w/app","timestamp":"2026-10-12T01:00:01.000Z","sessionId":"c1"}"#,
                r#"{"type":"ai-title","aiTitle":"Fixing the build","sessionId":"c1"}"#,
                r#"{"type":"user","message":{"role":"user","content":"second question"},"cwd":"/w/app","timestamp":"2026-10-12T01:05:00.000Z","sessionId":"c1"}"#,
            ],
        );
        // 打开就退出了：一句没问，不列。
        write(
            home,
            ".claude/projects/-w-app/c2.jsonl",
            &[
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"},"cwd":"/w/app","timestamp":"2026-10-12T01:00:00.000Z","sessionId":"c2"}"#,
            ],
        );
        write(
            home,
            ".claude/projects/-w-app/c1/subagents/agent-1.jsonl",
            &[
                r#"{"type":"user","message":{"role":"user","content":"sub task"},"cwd":"/w/app","timestamp":"2026-10-12T01:02:00.000Z","sessionId":"c1"}"#,
            ],
        );
        write(
            home,
            ".codex/sessions/2026/10/12/rollout-2026-10-12T01-00-00-t1.jsonl",
            &[
                r#"{"timestamp":"2026-10-12T02:00:00.000Z","type":"session_meta","payload":{"id":"t1","cwd":"/w/api","timestamp":"2026-10-12T02:00:00.000Z","model_provider":"openai","source":"cli","thread_source":"user"}}"#,
                r##"{"timestamp":"2026-10-12T02:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /w/api"},{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"##,
                r#"{"timestamp":"2026-10-12T02:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"add an endpoint"}]}}"#,
            ],
        );
        write(
            home,
            ".codex/sessions/2026/10/12/rollout-2026-10-12T01-00-00-t2.jsonl",
            &[
                r#"{"timestamp":"2026-10-12T02:00:00.000Z","type":"session_meta","payload":{"id":"t2","cwd":"/w/api","source":{"subagent":{}},"thread_source":"subagent"}}"#,
            ],
        );
        write(
            home,
            ".pi/agent/sessions/--w-cli--/2026-10-12T03-00-00_p1.jsonl",
            &[
                r#"{"type":"session","version":3,"id":"p1","timestamp":"2026-10-12T03:00:00.000Z","cwd":"/w/cli"}"#,
                r#"{"type":"message","id":"u1","timestamp":"2026-10-12T03:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"rename things"}]}}"#,
                r#"{"type":"session_info","id":"n1","name":"Renaming"}"#,
            ],
        );
        write(
            home,
            ".grok/sessions/w-site/g1/updates.jsonl",
            &[
                r#"{"timestamp":1789620532,"params":{"sessionId":"g1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"make it faster"}}}}"#,
            ],
        );
        write(
            home,
            ".grok/sessions/w-site/g1/summary.json",
            &[
                r#"{"info":{"id":"g1","cwd":"/w/site"},"session_summary":"Speeding up the site","created_at":"2026-10-12T04:00:00Z"}"#,
            ],
        );

        let mut db = Db::in_memory();
        scan(home, &mut db);
        let mut sessions = db.sessions().unwrap();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["c1", "g1", "p1", "t1"]);
        let claude = &sessions[0];
        assert_eq!(claude.agent, AgentKind::ClaudeCode);
        assert_eq!(claude.cwd.as_deref(), Some("/w/app"));
        assert_eq!(claude.prompt.as_deref(), Some("fix the build please"));
        assert_eq!(claude.label(), "Fixing the build");
        assert!(claude.started.is_some() && claude.active > 0);
        assert_eq!(sessions[1].label(), "Speeding up the site");
        assert_eq!(sessions[1].prompt.as_deref(), Some("make it faster"));
        assert_eq!(sessions[2].label(), "Renaming");
        assert_eq!(sessions[2].cwd.as_deref(), Some("/w/cli"));
        assert_eq!(sessions[3].prompt.as_deref(), Some("add an endpoint"));

        // 接着写：/rename 起了名字；第一句提问不变。
        let path = home.join(".claude/projects/-w-app/c1.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"type":"custom-title","customTitle":"Build","sessionId":"c1"}"#);
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        scan(home, &mut db);
        let claude = db
            .sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == "c1")
            .unwrap();
        assert_eq!(claude.label(), "Build");
        assert_eq!(claude.prompt.as_deref(), Some("fix the build please"));
    }
}
