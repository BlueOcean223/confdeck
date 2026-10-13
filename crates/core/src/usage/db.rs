//! 用量缓存 `~/.confdeck/cache/usage.db`。从会话记录算出来的，丢了、格式变了都能重扫，所以表结构一变就整个重建。
//!
//! - `files`：每个会话文件扫到哪（字节偏移）、当时的大小和修改时间，加上解析器要接着用的状态（JSON）。
//! - `requests`：每次请求一行，按 `key` 去重。
//! - `limits`：订阅额度的最新快照（Codex 的 `rate_limits`），按来源一行。
//! - `sessions`：每个会话文件的概况（会话页用），按文件一行，合并的规矩见 [`SessionPatch`]。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use super::{Request, SessionPatch, Tokens, UsageError};
use crate::agent::AgentKind;

/// 表结构的版本，改了表、或者解析规则改了（旧的请求要按新规则重算）就加一：旧库整个删掉重扫。
const SCHEMA: i32 = 2;

pub struct Db {
    conn: Connection,
    path: PathBuf,
}

/// 一个会话文件扫到哪。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileRow {
    pub size: u64,
    pub mtime: i64,
    /// 读完的最后一个整行之后的位置；下次从这里接着读。
    pub offset: u64,
    pub state: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, UsageError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| UsageError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path).map_err(db_error(path))?;
        Self::init(conn, path.to_path_buf())
    }

    #[cfg(test)]
    pub(super) fn in_memory() -> Self {
        Self::init(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    fn init(conn: Connection, path: PathBuf) -> Result<Self, UsageError> {
        let db = Self { conn, path };
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_error(&db.path))?;
        if version != SCHEMA {
            db.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS files;
                     DROP TABLE IF EXISTS requests;
                     DROP TABLE IF EXISTS limits;
                     DROP TABLE IF EXISTS sessions;",
                )
                .map_err(db_error(&db.path))?;
        }
        db.conn
            .execute_batch(&format!(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS files (
                     path TEXT PRIMARY KEY,
                     size INTEGER NOT NULL,
                     mtime INTEGER NOT NULL,
                     offset INTEGER NOT NULL,
                     state TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS requests (
                     key TEXT PRIMARY KEY,
                     agent TEXT NOT NULL,
                     at INTEGER NOT NULL,
                     session TEXT NOT NULL,
                     model TEXT NOT NULL,
                     provider TEXT,
                     input INTEGER NOT NULL,
                     cache_read INTEGER NOT NULL,
                     cache_write INTEGER NOT NULL,
                     output INTEGER NOT NULL,
                     reasoning INTEGER NOT NULL,
                     cost REAL
                 ) WITHOUT ROWID;
                 CREATE INDEX IF NOT EXISTS requests_at ON requests (at);
                 DROP TABLE IF EXISTS limits;
                 CREATE TABLE IF NOT EXISTS sessions (
                     path TEXT PRIMARY KEY,
                     agent TEXT NOT NULL,
                     id TEXT,
                     cwd TEXT,
                     prompt TEXT,
                     name TEXT,
                     title TEXT,
                     started INTEGER,
                     active INTEGER NOT NULL
                 );
                 PRAGMA user_version = {SCHEMA};"
            ))
            .map_err(db_error(&db.path))?;
        Ok(db)
    }

    pub(super) fn file(&self, path: &str) -> Result<Option<FileRow>, UsageError> {
        self.conn
            .query_row(
                "SELECT size, mtime, offset, state FROM files WHERE path = ?1",
                [path],
                |row| {
                    Ok(FileRow {
                        size: row.get::<_, i64>(0)? as u64,
                        mtime: row.get(1)?,
                        offset: row.get::<_, i64>(2)? as u64,
                        state: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(db_error(&self.path))
    }

    /// 一个文件扫完：新请求（已有的跳过）、扫到哪、会话概况（agent、更新、文件的修改时间）一起提交。返回新加了几条。
    pub(super) fn commit(
        &mut self,
        path: &str,
        file: &FileRow,
        requests: &[Request],
        session: Option<(AgentKind, &SessionPatch, i64)>,
    ) -> Result<usize, UsageError> {
        let db_path = self.path.clone();
        let tx = self.conn.transaction().map_err(db_error(&db_path))?;
        let mut added = 0;
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO requests
                     (key, agent, at, session, model, provider, input, cache_read, cache_write, output, reasoning, cost)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(db_error(&db_path))?;
            for request in requests {
                let tokens = &request.tokens;
                added += insert
                    .execute(params![
                        request.key,
                        request.agent.id(),
                        request.at,
                        request.session,
                        request.model,
                        request.provider,
                        tokens.input as i64,
                        tokens.cache_read as i64,
                        tokens.cache_write as i64,
                        tokens.output as i64,
                        tokens.reasoning as i64,
                        request.cost,
                    ])
                    .map_err(db_error(&db_path))?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime, offset, state) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![path, file.size as i64, file.mtime, file.offset as i64, file.state],
        )
        .map_err(db_error(&db_path))?;
        if let Some((agent, patch, active)) = session {
            tx.execute(
                "INSERT INTO sessions (path, agent, id, cwd, prompt, name, title, started, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULLIF(?6, ''), ?7, ?8, ?9)
                 ON CONFLICT (path) DO UPDATE SET
                     id = COALESCE(sessions.id, excluded.id),
                     cwd = COALESCE(sessions.cwd, excluded.cwd),
                     prompt = COALESCE(sessions.prompt, excluded.prompt),
                     name = CASE WHEN ?6 IS NULL THEN sessions.name ELSE NULLIF(?6, '') END,
                     title = COALESCE(excluded.title, sessions.title),
                     started = COALESCE(sessions.started, excluded.started),
                     active = MAX(sessions.active, excluded.active)",
                params![
                    path,
                    agent.id(),
                    patch.id,
                    patch.cwd,
                    patch.prompt,
                    patch.name,
                    patch.title,
                    patch.started,
                    active
                ],
            )
            .map_err(db_error(&db_path))?;
        }
        tx.commit().map_err(db_error(&db_path))?;
        Ok(added)
    }

    /// 全部会话的概况，最近活动的在前。还没读到 ID 的、一句话没问过（没提问、没名字、没标题）的、以后去掉了的 agent 的不要。
    pub fn sessions(&self) -> Result<Vec<crate::session::Session>, UsageError> {
        let mut query = self
            .conn
            .prepare_cached(
                "SELECT path, agent, id, cwd, prompt, name, title, started, active
                 FROM sessions
                 WHERE id IS NOT NULL AND COALESCE(prompt, name, title) IS NOT NULL
                 ORDER BY active DESC",
            )
            .map_err(db_error(&self.path))?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    crate::session::Session {
                        agent: AgentKind::ClaudeCode,
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        id: row.get(2)?,
                        cwd: row.get(3)?,
                        prompt: row.get(4)?,
                        name: row.get(5)?,
                        title: row.get(6)?,
                        started: row.get(7)?,
                        active: row.get(8)?,
                    },
                ))
            })
            .map_err(db_error(&self.path))?;
        let mut sessions = Vec::new();
        for row in rows {
            let (agent, mut session) = row.map_err(db_error(&self.path))?;
            let Some(kind) = AgentKind::ALL.into_iter().find(|kind| kind.id() == agent) else {
                continue;
            };
            session.agent = kind;
            sessions.push(session);
        }
        Ok(sessions)
    }

    /// `since`（毫秒）以后的请求，按时间排；`None` 是全部。
    pub fn requests(&self, since: Option<i64>) -> Result<Vec<Request>, UsageError> {
        let mut query = self
            .conn
            .prepare_cached(
                "SELECT key, agent, at, session, model, provider, input, cache_read, cache_write, output, reasoning, cost
                 FROM requests WHERE at >= ?1 ORDER BY at",
            )
            .map_err(db_error(&self.path))?;
        let rows = query
            .query_map([since.unwrap_or(i64::MIN)], |row| {
                let agent: String = row.get(1)?;
                Ok((
                    agent,
                    Request {
                        key: row.get(0)?,
                        agent: AgentKind::ClaudeCode,
                        at: row.get(2)?,
                        session: row.get(3)?,
                        model: row.get(4)?,
                        provider: row.get(5)?,
                        tokens: Tokens {
                            input: row.get::<_, i64>(6)? as u64,
                            cache_read: row.get::<_, i64>(7)? as u64,
                            cache_write: row.get::<_, i64>(8)? as u64,
                            output: row.get::<_, i64>(9)? as u64,
                            reasoning: row.get::<_, i64>(10)? as u64,
                        },
                        cost: row.get(11)?,
                    },
                ))
            })
            .map_err(db_error(&self.path))?;
        let mut requests = Vec::new();
        for row in rows {
            let (agent, mut request) = row.map_err(db_error(&self.path))?;
            // 以后去掉了的 agent：跳过，不报错。
            let Some(kind) = AgentKind::ALL.into_iter().find(|kind| kind.id() == agent) else {
                continue;
            };
            request.agent = kind;
            requests.push(request);
        }
        Ok(requests)
    }
}

fn db_error(path: &Path) -> impl Fn(rusqlite::Error) -> UsageError + '_ {
    move |source| UsageError::Db {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::{Db, FileRow};
    use crate::agent::AgentKind;
    use crate::usage::{Request, Tokens};

    fn request(key: &str, at: i64) -> Request {
        Request {
            key: key.into(),
            agent: AgentKind::Codex,
            at,
            session: "s".into(),
            model: "m".into(),
            provider: Some("custom".into()),
            tokens: Tokens {
                input: 1,
                cache_read: 2,
                cache_write: 3,
                output: 4,
                reasoning: 1,
            },
            cost: Some(0.5),
        }
    }

    fn row(offset: u64) -> FileRow {
        FileRow {
            size: offset,
            mtime: 1,
            offset,
            state: "{}".into(),
        }
    }

    #[test]
    fn requests_are_kept_once_and_read_back_in_order() {
        let mut db = Db::in_memory();
        let added = db
            .commit("a", &row(10), &[request("k2", 20), request("k1", 10)], None)
            .unwrap();
        assert_eq!(added, 2);
        // 同一次请求在另一个文件里又出现（resume、fork 复制了历史）：不再加。
        let added = db.commit("b", &row(5), &[request("k1", 10)], None).unwrap();
        assert_eq!(added, 0);

        let all = db.requests(None).unwrap();
        assert_eq!(all, vec![request("k1", 10), request("k2", 20)]);
        assert_eq!(db.requests(Some(15)).unwrap(), vec![request("k2", 20)]);
        assert_eq!(db.file("a").unwrap(), Some(row(10)));
        assert_eq!(db.file("c").unwrap(), None);
    }
}
