//! Task persistence over SQLite.
//!
//! The store owns one connection and runs every statement on the thread that owns it;
//! callers clone a handle and send work in. Nothing here knows about the network or the
//! user interface.
//!
//! Durability, stated plainly: the database runs in WAL with
//! `synchronous = NORMAL`, so a power cut can lose the most recent transaction. That is
//! acceptable because the **file** is the source of truth for bytes, and the record is
//! the source of truth for *which* bytes are known good. A crash can therefore forget
//! the last few hundred kilobytes of progress and re-download them; it never produces a
//! file the record calls complete when it is not (see the engine's verified finish).

use omad_core::{TaskId, TaskStatus};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("no task with id {0}")]
    NotFound(TaskId),
    #[error("the database path {path:?} has no usable directory: {reason}")]
    Path { path: PathBuf, reason: String },
}

/// One row of a task, with the engine's private bookkeeping.
#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: TaskId,
    pub url: String,
    pub filename: String,
    pub dest: String,
    pub status: TaskStatus,
    pub total_bytes: Option<u64>,
    pub received_bytes: u64,
    /// Bytes per second as the engine last measured it, so a reader that is not the one
    /// downloading (the bar, the CLI, another window) can show a live rate.
    pub speed: u64,
    pub connections: u8,
    pub etag: Option<String>,
    /// Absolute path of the partial file. Never derived from user text at read time;
    /// it was computed once, inside a validated root, when the task was created.
    pub file_path: String,
    pub error: Option<String>,
    /// Chunk boundaries and per-chunk progress, as JSON. Engine-private.
    pub plan: Option<String>,
    /// Expected digest, when the server published a validator we can verify with.
    pub expected_sha256: Option<String>,
    /// The user allowed this task to reach a loopback or private address. Kept with the
    /// task so a resume honours the decision instead of asking again, or silently refusing.
    pub allow_private: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TaskRecord {
    pub fn snapshot(&self) -> omad_core::TaskSnapshot {
        omad_core::TaskSnapshot {
            id: self.id,
            url: self.url.clone(),
            filename: self.filename.clone(),
            dest: self.dest.clone(),
            status: self.status,
            total_bytes: self.total_bytes,
            received_bytes: self.received_bytes,
            speed: self.speed,
            connections: self.connections,
            etag: self.etag.clone(),
            error: self.error.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// What is needed to create a row. The path is already resolved and validated.
#[derive(Clone, Debug)]
pub struct NewTask {
    pub url: String,
    pub filename: String,
    pub dest: String,
    pub file_path: String,
    pub connections: u8,
    pub allow_private: bool,
}

/// What a restart needs from the record: the chunk plan and the digest it must match.
pub type SavedPlan = (Option<String>, Option<String>);

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

const COLUMNS: &str = "id, url, filename, dest, status, total_bytes, received_bytes,
        speed, connections, etag, file_path, error, plan, expected_sha256, allow_private,
        created_at, updated_at";

impl Store {
    /// Opens or creates the database at `path`, creating its directory if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Path {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
            restrict_permissions(parent);
        }
        let conn = Connection::open(path)?;
        configure(&conn)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    /// An in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                url             TEXT    NOT NULL,
                filename        TEXT    NOT NULL,
                dest            TEXT    NOT NULL,
                status          TEXT    NOT NULL,
                total_bytes     INTEGER,
                received_bytes  INTEGER NOT NULL DEFAULT 0,
                speed           INTEGER NOT NULL DEFAULT 0,
                connections     INTEGER NOT NULL DEFAULT 8,
                etag            TEXT,
                file_path       TEXT    NOT NULL,
                error           TEXT,
                plan            TEXT,
                expected_sha256 TEXT,
                allow_private   INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS tasks_status ON tasks(status);
            -- The browser profiles the user has paired with, one row per origin.
            CREATE TABLE IF NOT EXISTS paired_origins (
                origin     TEXT PRIMARY KEY,
                secret     TEXT NOT NULL DEFAULT '',
                granted_at INTEGER NOT NULL
            );
            "#,
        )?;
        // A store from before this column existed gains it here; `user_version` would be
        // the tidier record, but one additive column does not need a migration framework.
        let has_column: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'allow_private'")?
            .exists([])?;
        if !has_column {
            conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN allow_private INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        let has_secret: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('paired_origins') WHERE name = 'secret'")?
            .exists([])?;
        if !has_secret {
            conn.execute_batch(
                "ALTER TABLE paired_origins ADD COLUMN secret TEXT NOT NULL DEFAULT ''",
            )?;
        }
        let has_secret: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('paired_origins') WHERE name = 'secret'")?
            .exists([])?;
        if !has_secret {
            conn.execute_batch(
                "ALTER TABLE paired_origins ADD COLUMN secret TEXT NOT NULL DEFAULT ''",
            )?;
        }
        let has_speed: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'speed'")?
            .exists([])?;
        if !has_speed {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN speed INTEGER NOT NULL DEFAULT 0")?;
        }
        Ok(())
    }

    pub fn create(&self, new: NewTask) -> Result<TaskRecord, StoreError> {
        let now = now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (url, filename, dest, status, total_bytes, received_bytes,
                                connections, file_path, allow_private, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5, ?6, ?7, ?8, ?8)",
            params![
                new.url,
                new.filename,
                new.dest,
                TaskStatus::Queued.as_str(),
                new.connections,
                new.file_path,
                new.allow_private as i64,
                now
            ],
        )?;
        let id = TaskId(conn.last_insert_rowid() as u64);
        drop(conn);
        self.get(id)?.ok_or(StoreError::NotFound(id))
    }

    pub fn get(&self, id: TaskId) -> Result<Option<TaskRecord>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached(&format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"))?;
        Ok(stmt.query_row([id.0 as i64], map_row).optional()?)
    }

    pub fn list(&self) -> Result<Vec<TaskRecord>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached(&format!("SELECT {COLUMNS} FROM tasks ORDER BY id DESC"))?;
        Ok(stmt
            .query_map([], map_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Tasks a previous process left unfinished. Their record is trusted, so they
    /// resume rather than restart.
    pub fn resumable(&self) -> Result<Vec<TaskRecord>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE status IN ('running', 'queued', 'failed')
             ORDER BY id ASC"
        ))?;
        Ok(stmt
            .query_map([], map_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn set_status(
        &self,
        id: TaskId,
        status: TaskStatus,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE tasks SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![status.as_str(), error, now(), id.0 as i64],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        Ok(())
    }

    /// Records the probe's answer and the chunk plan in one statement, so a crash
    /// leaves either the old state or the new one.
    pub fn set_plan(
        &self,
        id: TaskId,
        total: Option<u64>,
        etag: Option<&str>,
        expected_sha256: Option<&str>,
        plan: &str,
        connections: u8,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET total_bytes = ?1, etag = ?2, expected_sha256 = ?3, plan = ?4,
                              connections = ?5, updated_at = ?6
             WHERE id = ?7",
            params![
                total.map(|v| v as i64),
                etag,
                expected_sha256,
                plan,
                connections,
                now(),
                id.0 as i64
            ],
        )?;
        Ok(())
    }

    /// Advances the record. Called in batches, never per byte.
    pub fn set_progress(
        &self,
        id: TaskId,
        received: u64,
        total: Option<u64>,
        speed: u64,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET received_bytes = ?1, total_bytes = coalesce(?2, total_bytes),
                              speed = ?3, updated_at = ?4
             WHERE id = ?5",
            params![
                received as i64,
                total.map(|v| v as i64),
                speed as i64,
                now(),
                id.0 as i64
            ],
        )?;
        Ok(())
    }

    pub fn plan_of(&self, id: TaskId) -> Result<Option<SavedPlan>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT plan, expected_sha256 FROM tasks WHERE id = ?1",
                [id.0 as i64],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn delete(&self, id: TaskId) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tasks WHERE id = ?1", [id.0 as i64])?;
        Ok(())
    }

    pub fn remember_origin(&self, origin: &str, secret: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO paired_origins (origin, secret, granted_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(origin) DO UPDATE SET secret = excluded.secret",
            params![origin, secret, now()],
        )?;
        Ok(())
    }

    /// The origins this profile trusts, with the secret each one proves itself with.
    pub fn paired_origins(&self) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT origin, secret FROM paired_origins ORDER BY granted_at")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let status: String = r.get(4)?;
    Ok(TaskRecord {
        id: TaskId(r.get::<_, i64>(0)? as u64),
        url: r.get(1)?,
        filename: r.get(2)?,
        dest: r.get(3)?,
        status: TaskStatus::from_storage(&status).unwrap_or(TaskStatus::Failed),
        total_bytes: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        received_bytes: r.get::<_, i64>(6)? as u64,
        speed: r.get::<_, i64>(7)? as u64,
        connections: r.get::<_, i64>(8)?.clamp(1, 32) as u8,
        etag: r.get(9)?,
        file_path: r.get(10)?,
        error: r.get(11)?,
        plan: r.get(12)?,
        expected_sha256: r.get(13)?,
        allow_private: r.get::<_, i64>(14)? != 0,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

fn configure(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) {}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task(name: &str) -> NewTask {
        NewTask {
            url: "https://example.com/a.bin".into(),
            filename: name.into(),
            dest: "downloads".into(),
            file_path: format!("/tmp/{name}"),
            connections: 8,
            allow_private: false,
        }
    }

    #[test]
    fn creates_reads_lists_and_deletes() {
        let store = Store::open_in_memory().unwrap();
        let a = store.create(new_task("a.bin")).unwrap();
        let b = store.create(new_task("b.bin")).unwrap();
        assert_eq!(a.status, TaskStatus::Queued);
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(store.get(b.id).unwrap().unwrap().filename, "b.bin");
        store.delete(a.id).unwrap();
        assert!(store.get(a.id).unwrap().is_none());
    }

    #[test]
    fn a_plan_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/omalldownloader.sqlite3");
        let id = {
            let store = Store::open(&path).unwrap();
            let t = store.create(new_task("a.bin")).unwrap();
            store
                .set_plan(
                    t.id,
                    Some(1000),
                    Some("\"etag1\""),
                    None,
                    "{\"chunks\":[]}",
                    8,
                )
                .unwrap();
            store.set_progress(t.id, 400, Some(1000), 2048).unwrap();
            store.set_status(t.id, TaskStatus::Running, None).unwrap();
            t.id
        };
        let store = Store::open(&path).unwrap();
        let t = store.get(id).unwrap().unwrap();
        assert_eq!(t.total_bytes, Some(1000));
        assert_eq!(t.received_bytes, 400);
        assert_eq!(t.speed, 2048);
        assert_eq!(t.etag.as_deref(), Some("\"etag1\""));
        assert_eq!(t.status, TaskStatus::Running);
        assert_eq!(store.resumable().unwrap().len(), 1);
    }

    #[test]
    fn paired_origins_are_remembered_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .remember_origin("chrome-extension://abc", "secret-1")
            .unwrap();
        // A second grant replaces the secret: the newest one is what the extension holds.
        store
            .remember_origin("chrome-extension://abc", "secret-2")
            .unwrap();
        assert_eq!(
            store.paired_origins().unwrap(),
            vec![("chrome-extension://abc".to_string(), "secret-2".to_string())]
        );
    }

    #[test]
    fn a_missing_task_is_an_error_when_written_to() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .set_status(TaskId(99), TaskStatus::Paused, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }
}
