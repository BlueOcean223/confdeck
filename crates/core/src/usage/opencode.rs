//! OpenCode：`~/.local/share/opencode/opencode.db`（SQLite，WAL 模式）。照 fork 的 `session_usage_opencode.rs` 写，本机没装。
//!
//! - 新版是 `session_v2` / `session_message`（消息类型在 `type` 列，`compaction` 也带用量），没迁移的是 `session` / `message`
//!   （`data.role == "assistant"`）。消息的 `data` 里：`tokens`（`input` 不含缓存，`output` 不含推理，`reasoning`，
//!   `cache.read` / `cache.write`）、`cost`、`time.created` / `time.completed`；
//!   模型旧版是 `modelID` + `providerID`，新版是 `model: { id, providerID }`。
//! - 只读完成了的（`time.completed`；`compaction` 看 `status`）：没完成的 token 只有半截。
//! - fork 出来的会话复制了父会话的消息，父会话里已经算过：新版的复制品 ID 是 `msg_<事件 id>_<序号>`，
//!   旧版复制品的创建时间早于会话本身。
//! - 增量：记着读到的最大 `time_updated`，下次只读这之后改过的消息；有没完成的，从它之前重读，靠键去重。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use super::db::FileRow;
use super::scan::Report;
use super::{Db, Request, Tokens, UsageError, clean_model};
use crate::agent::AgentKind;

pub(super) fn path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

pub(super) fn scan(home: &Path, db: &mut Db, report: &mut Report) {
    let path = path(home);
    if !path.exists() {
        return;
    }
    report.files += 1;
    match scan_db(db, &path) {
        Ok(Some(added)) => {
            report.read += 1;
            report.added += added;
        }
        Ok(None) => {}
        Err(err) => report.errors.push(format!(
            "{} {}: {err}",
            AgentKind::Opencode.id(),
            path.display()
        )),
    }
}

fn scan_db(db: &mut Db, path: &Path) -> Result<Option<usize>, UsageError> {
    // 新写的先进 `-wal`，checkpoint 以后主文件才变：两个都要看。
    let (mut size, mut mtime) = (0, 0);
    for file in [path.to_path_buf(), path.with_extension("db-wal")] {
        if let Ok(meta) = std::fs::metadata(&file) {
            size += meta.len();
            mtime = mtime.max(
                meta.modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |time| time.as_nanos() as i64),
            );
        }
    }
    let key = path.to_string_lossy();
    let previous = db.file(&key)?;
    if previous
        .as_ref()
        .is_some_and(|row| row.size == size && row.mtime == mtime)
    {
        return Ok(None);
    }
    let since = previous
        .and_then(|row| row.state.parse().ok())
        .unwrap_or(i64::MIN);
    let source = |source| UsageError::Db {
        path: path.to_path_buf(),
        source,
    };
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(source)?;
    let (requests, watermark) = read(&conn, since).map_err(source)?;
    let row = FileRow {
        size,
        mtime,
        offset: 0,
        state: watermark.to_string(),
    };
    db.commit(&key, &row, &requests, None, None).map(Some)
}

/// `since` 之后改过的消息里的请求，和下次从哪儿读起。
fn read(conn: &Connection, since: i64) -> rusqlite::Result<(Vec<Request>, i64)> {
    let v2 = has_table(conn, "session_v2")? && has_table(conn, "session_message")?;
    let (sessions, messages) = if v2 {
        ("session_v2", "session_message")
    } else {
        ("session", "message")
    };
    let updated = if has_column(conn, messages, "time_updated")? {
        "time_updated"
    } else {
        "time_created"
    };
    let kind = if v2 { "m.type" } else { "NULL" };
    let created = if has_column(conn, sessions, "time_created")? {
        "s.time_created"
    } else {
        "NULL"
    };
    let fork = if v2 && has_column(conn, sessions, "fork_session_id")? {
        "s.fork_session_id IS NOT NULL"
    } else {
        "0"
    };
    let only = if v2 {
        "AND m.type IN ('assistant', 'compaction')"
    } else {
        ""
    };
    let mut query = conn.prepare(&format!(
        "SELECT m.id, m.session_id, m.data, {kind}, m.{updated}, {created}, {fork}
         FROM {messages} m LEFT JOIN {sessions} s ON s.id = m.session_id
         WHERE m.{updated} > ?1 {only}
         ORDER BY m.{updated}"
    ))?;
    let mut rows = query.query([since])?;
    let mut requests = Vec::new();
    let mut newest = since;
    let mut unfinished: Option<i64> = None;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let session: String = row.get(1)?;
        let data: String = row.get(2)?;
        let kind: Option<String> = row.get(3)?;
        let updated: i64 = row.get(4)?;
        let session_created: Option<i64> = row.get(5)?;
        let fork: bool = row.get(6)?;
        newest = newest.max(updated);
        if fork && is_fork_copy(&id) {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if !v2 && data["role"] != "assistant" {
            continue;
        }
        let created = data["time"]["created"].as_i64();
        if let (Some(session), Some(created)) = (session_created, created)
            && created < session
        {
            continue;
        }
        if data.get("tokens").is_none() {
            continue;
        }
        let finished = if kind.as_deref() == Some("compaction") {
            matches!(data["status"].as_str(), Some("completed" | "failed"))
        } else {
            !data["time"]["completed"].is_null()
        };
        if !finished {
            unfinished = Some(unfinished.map_or(updated, |at| at.min(updated)));
            continue;
        }
        if let Some(request) = request(&id, session, &data, created.unwrap_or(updated)) {
            requests.push(request);
        }
    }
    let watermark = match unfinished {
        Some(at) => newest.min(at - 1),
        None => newest,
    };
    Ok((requests, watermark))
}

fn request(id: &str, session: String, data: &Value, at: i64) -> Option<Request> {
    let tokens = &data["tokens"];
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    let reasoning = count(&tokens["reasoning"]);
    let tokens = Tokens {
        input: count(&tokens["input"]),
        cache_read: count(&tokens["cache"]["read"]),
        cache_write: count(&tokens["cache"]["write"]),
        output: count(&tokens["output"]) + reasoning,
        reasoning,
    };
    if tokens.is_zero() {
        return None;
    }
    let model = &data["model"];
    let model_id = data["modelID"]
        .as_str()
        .or(model.as_str())
        .or(model["id"].as_str())
        .unwrap_or_default();
    let provider = data["providerID"]
        .as_str()
        .or(model["providerID"].as_str())
        .map(str::to_string);
    Some(Request {
        key: format!("opencode:{id}"),
        agent: AgentKind::Opencode,
        at,
        session,
        model: clean_model(model_id),
        provider,
        tokens,
        cost: data["cost"].as_f64().filter(|cost| *cost > 0.),
    })
}

/// fork 时从父会话原样复制的消息：ID 是 `msg_<事件 id>_<父会话序号>`；正常的是 `msg_` 加 26 位字母数字。
fn is_fork_copy(id: &str) -> bool {
    id.strip_prefix("msg_")
        .and_then(|rest| rest.rsplit_once('_'))
        .is_some_and(|(_, seq)| !seq.is_empty() && seq.bytes().all(|b| b.is_ascii_digit()))
}

fn has_table(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::json;

    use super::read;
    use crate::usage::Tokens;

    fn message(created: i64, completed: Option<i64>) -> String {
        json!({
            "cost": 0.0023,
            "tokens": { "input": 1373, "output": 96, "reasoning": 4, "cache": { "read": 391040, "write": 0 } },
            "model": { "id": "deepseek-v4.1-flash", "providerID": "confdeck-ds" },
            "time": { "created": created, "completed": completed }
        })
        .to_string()
    }

    #[test]
    fn v2_skips_unfinished_and_forked_copies() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT, time_created INTEGER, fork_session_id TEXT);
             CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO session_v2 VALUES ('s1', 100, NULL), ('s2', 500, 's1');",
        )
        .unwrap();
        let insert = |id: &str, session: &str, kind: &str, updated: i64, data: String| {
            conn.execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                rusqlite::params![id, session, kind, updated, data],
            )
            .unwrap();
        };
        insert("msg_a", "s1", "assistant", 200, message(200, Some(210)));
        insert("msg_b", "s1", "user", 220, message(220, Some(230)));
        insert("msg_c", "s1", "assistant", 300, message(300, None));
        insert("msg_e1_3", "s2", "assistant", 600, message(200, Some(210)));
        insert("msg_d", "s2", "assistant", 700, message(700, Some(710)));

        let (requests, watermark) = read(&conn, i64::MIN).unwrap();
        let keys: Vec<_> = requests.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["opencode:msg_a", "opencode:msg_d"]);
        assert_eq!(watermark, 299, "重读还没完成的那条");
        let first = &requests[0];
        assert_eq!(first.provider.as_deref(), Some("confdeck-ds"));
        assert_eq!(first.model, "deepseek-v4.1-flash");
        assert_eq!(first.at, 200);
        assert_eq!(
            first.tokens,
            Tokens {
                input: 1373,
                cache_read: 391040,
                cache_write: 0,
                output: 100,
                reasoning: 4
            }
        );

        let (requests, _) = read(&conn, 699).unwrap();
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn v1_reads_assistant_messages() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO session VALUES ('fork', 5000, 9000);",
        )
        .unwrap();
        let data = |created: i64, role: &str| {
            json!({
                "role": role, "modelID": "m", "providerID": "openai", "cost": 0.01,
                "tokens": { "input": 100, "output": 10 },
                "time": { "created": created, "completed": created + 1 }
            })
            .to_string()
        };
        conn.execute(
            "INSERT INTO message VALUES ('copied', 'fork', 5000, ?1), ('new', 'fork', 6000, ?2), ('ask', 'fork', 6500, ?3)",
            [data(4000, "assistant"), data(6000, "assistant"), data(6500, "user")],
        )
        .unwrap();
        let (requests, watermark) = read(&conn, i64::MIN).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].key, "opencode:new");
        assert_eq!(requests[0].provider.as_deref(), Some("openai"));
        assert_eq!(requests[0].cost, Some(0.01));
        assert_eq!(watermark, 6500);
    }
}
