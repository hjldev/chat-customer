use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::models::{ChatMessage, ChatSession};

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    Task(tokio::task::JoinError),
    NotFound,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sqlite(e) => write!(f, "SQLite error: {}", e),
            DbError::Task(e) => write!(f, "Background task error: {}", e),
            DbError::NotFound => write!(f, "Record not found"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(value: rusqlite::Error) -> Self {
        DbError::Sqlite(value)
    }
}

impl From<tokio::task::JoinError> for DbError {
    fn from(value: tokio::task::JoinError) -> Self {
        DbError::Task(value)
    }
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        Self::init_connection(conn)
    }

    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::init_connection(conn)
    }

    fn init_connection(conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                visitor_token TEXT NOT NULL,
                visitor_name TEXT NOT NULL,
                visitor_contact TEXT NOT NULL DEFAULT '',
                visitor_meta TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'open',
                unread_count INTEGER NOT NULL DEFAULT 0,
                last_message TEXT,
                last_message_sender TEXT,
                last_message_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                sender TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at DESC);
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn run_blocking<F, T>(&self, f: F) -> Result<T, DbError>
    where
        F: FnOnce(&mut Connection) -> Result<T, DbError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().expect("SQLite mutex poisoned");
            f(&mut guard)
        })
        .await?
    }

    pub async fn create_session(
        &self,
        visitor_name: String,
        visitor_contact: String,
        visitor_meta: String,
    ) -> Result<(ChatSession, String), DbError> {
        let id = Uuid::new_v4().to_string();
        let visitor_token = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let id_clone = id.clone();
        let token_clone = visitor_token.clone();
        let now_clone = now.clone();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO sessions (
                    id, visitor_token, visitor_name, visitor_contact, visitor_meta,
                    status, unread_count, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 'open', 0, ?6, ?7)",
                params![
                    id_clone,
                    token_clone,
                    visitor_name,
                    visitor_contact,
                    visitor_meta,
                    now_clone,
                    now_clone
                ],
            )?;

            let session = ChatSession {
                id: id_clone,
                visitor_token: token_clone.clone(),
                visitor_name,
                visitor_contact,
                visitor_meta,
                status: "open".to_string(),
                unread_count: 0,
                last_message: None,
                last_message_sender: None,
                last_message_at: None,
                created_at: now_clone.clone(),
                updated_at: now_clone,
                online: false,
            };

            Ok((session, token_clone))
        })
        .await
    }

    pub async fn get_session(&self, session_id: String) -> Result<Option<ChatSession>, DbError> {
        self.run_blocking(move |conn| query_session_by_id(conn, &session_id))
            .await
    }

    pub async fn list_sessions(&self, status_filter: Option<String>) -> Result<Vec<ChatSession>, DbError> {
        self.run_blocking(move |conn| {
            let mut sessions = Vec::new();
            if let Some(status) = status_filter {
                let mut stmt = conn.prepare(
                    "SELECT id, visitor_token, visitor_name, visitor_contact, visitor_meta,
                            status, unread_count, last_message, last_message_sender,
                            last_message_at, created_at, updated_at
                     FROM sessions
                     WHERE status = ?1
                     ORDER BY COALESCE(last_message_at, created_at) DESC",
                )?;
                let rows = stmt.query_map(params![status], row_to_session)?;
                for row in rows {
                    sessions.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, visitor_token, visitor_name, visitor_contact, visitor_meta,
                            status, unread_count, last_message, last_message_sender,
                            last_message_at, created_at, updated_at
                     FROM sessions
                     ORDER BY COALESCE(last_message_at, created_at) DESC",
                )?;
                let rows = stmt.query_map([], row_to_session)?;
                for row in rows {
                    sessions.push(row?);
                }
            }
            Ok(sessions)
        })
        .await
    }

    pub async fn update_visitor_profile(
        &self,
        session_id: String,
        visitor_name: String,
        visitor_contact: String,
    ) -> Result<Option<ChatSession>, DbError> {
        let now = Utc::now().to_rfc3339();
        self.run_blocking(move |conn| {
            let changed = conn.execute(
                "UPDATE sessions
                 SET visitor_name = ?1, visitor_contact = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![visitor_name, visitor_contact, now, session_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            query_session_by_id(conn, &session_id)
        })
        .await
    }

    pub async fn update_session_status(
        &self,
        session_id: String,
        status: String,
    ) -> Result<Option<ChatSession>, DbError> {
        let now = Utc::now().to_rfc3339();
        self.run_blocking(move |conn| {
            let changed = conn.execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, session_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            query_session_by_id(conn, &session_id)
        })
        .await
    }

    pub async fn mark_session_read(&self, session_id: String) -> Result<Option<ChatSession>, DbError> {
        self.run_blocking(move |conn| {
            let changed = conn.execute(
                "UPDATE sessions SET unread_count = 0 WHERE id = ?1",
                params![session_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            query_session_by_id(conn, &session_id)
        })
        .await
    }

    pub async fn delete_session(&self, session_id: String) -> Result<bool, DbError> {
        self.run_blocking(move |conn| {
            let deleted = conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
            Ok(deleted > 0)
        })
        .await
    }

    pub async fn insert_message(
        &self,
        session_id: String,
        sender: String,
        content: String,
    ) -> Result<(ChatMessage, ChatSession), DbError> {
        let now = Utc::now().to_rfc3339();
        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            let exists: Option<String> = tx
                .query_row(
                    "SELECT id FROM sessions WHERE id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;

            if exists.is_none() {
                return Err(DbError::NotFound);
            }

            tx.execute(
                "INSERT INTO messages (session_id, sender, content, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![session_id, sender, content, now],
            )?;
            let message_id = tx.last_insert_rowid();

            // Truncate preview safely on UTF-8 char boundary
            let preview: String = content.chars().take(120).collect();

            if sender == "visitor" {
                tx.execute(
                    "UPDATE sessions
                     SET unread_count = unread_count + 1,
                         status = 'open',
                         last_message = ?1,
                         last_message_sender = ?2,
                         last_message_at = ?3,
                         updated_at = ?3
                     WHERE id = ?4",
                    params![preview, sender, now, session_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE sessions
                     SET unread_count = 0,
                         status = 'open',
                         last_message = ?1,
                         last_message_sender = ?2,
                         last_message_at = ?3,
                         updated_at = ?3
                     WHERE id = ?4",
                    params![preview, sender, now, session_id],
                )?;
            }

            let session = query_session_by_id(&tx, &session_id)?.ok_or(DbError::NotFound)?;
            tx.commit()?;

            let msg = ChatMessage {
                id: message_id,
                session_id,
                sender,
                content,
                created_at: now,
            };

            Ok((msg, session))
        })
        .await
    }

    pub async fn list_messages(
        &self,
        session_id: String,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, DbError> {
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, sender, content, created_at
                 FROM (
                    SELECT id, session_id, sender, content, created_at
                    FROM messages
                    WHERE session_id = ?1
                    ORDER BY id DESC
                    LIMIT ?2
                 )
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![session_id, limit as i64], |row| {
                Ok(ChatMessage {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    sender: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?;

            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(messages)
        })
        .await
    }
}

fn query_session_by_id(conn: &Connection, session_id: &str) -> Result<Option<ChatSession>, DbError> {
    let session = conn
        .query_row(
            "SELECT id, visitor_token, visitor_name, visitor_contact, visitor_meta,
                    status, unread_count, last_message, last_message_sender,
                    last_message_at, created_at, updated_at
             FROM sessions
             WHERE id = ?1",
            params![session_id],
            row_to_session,
        )
        .optional()?;
    Ok(session)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatSession> {
    Ok(ChatSession {
        id: row.get(0)?,
        visitor_token: row.get(1)?,
        visitor_name: row.get(2)?,
        visitor_contact: row.get(3)?,
        visitor_meta: row.get(4)?,
        status: row.get(5)?,
        unread_count: row.get(6)?,
        last_message: row.get(7)?,
        last_message_sender: row.get(8)?,
        last_message_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        online: false,
    })
}
