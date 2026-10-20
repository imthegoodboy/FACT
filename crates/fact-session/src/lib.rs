//! SQLite-backed session metadata, validation, export and cold-recovery snapshots.
//!
//! The daemon owns live processes. The database stores enough information to
//! restore a *visual* layout after a reboot: pane identities, titles, profile and
//! working directory, plus a bounded ANSI snapshot per pane. Commands are never
//! persisted or replayed.

use fact_layout::{SessionLayout, TabLayout};
use fact_protocol::{validate_session_name, SessionSummary};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

pub const EXPORT_FORMAT: &str = "fact-session";
pub const EXPORT_VERSION: u32 = 1;
pub const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CWD_CHARS: usize = 4096;
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session '{0}' was not found")]
    NotFound(String),
    #[error("session '{0}' already exists")]
    AlreadyExists(String),
    #[error("session is inconsistent: {0}")]
    Invalid(String),
    #[error("session export is invalid: {0}")]
    Corrupt(String),
    #[error("session database uses unsupported schema version {0}")]
    Schema(i64),
    #[error("export is too large ({size} bytes)")]
    TooLarge { size: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub ansi: String,
    pub cols: u16,
    pub rows: u16,
    pub sha256: String,
    pub format_version: u32,
    pub captured_at_ms: i64,
    pub truncated: bool,
}

impl PaneSnapshot {
    /// Validate size, hash and dimensions before this snapshot is replayed.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.format_version != 1 {
            return Err(SessionError::Corrupt(format!(
                "unsupported snapshot format version {}",
                self.format_version
            )));
        }
        if self.ansi.len() > MAX_SNAPSHOT_BYTES {
            return Err(SessionError::Corrupt("snapshot is too large".into()));
        }
        if self.ansi.contains('\0') {
            return Err(SessionError::Corrupt("snapshot contains NUL".into()));
        }
        if self.cols == 0 || self.rows == 0 || self.cols > 1000 || self.rows > 1000 {
            return Err(SessionError::Corrupt(
                "snapshot dimensions are out of range".into(),
            ));
        }
        let expected = sha256_hex(self.ansi.as_bytes());
        if !self.sha256.eq_ignore_ascii_case(&expected) {
            return Err(SessionError::Corrupt(
                "snapshot checksum does not match its contents".into(),
            ));
        }
        Ok(())
    }

    /// Build and checksum a snapshot from a terminal serialization.
    pub fn new(
        ansi: String,
        cols: u16,
        rows: u16,
        format_version: u32,
        captured_at_ms: i64,
        truncated: bool,
    ) -> Self {
        let sha256 = sha256_hex(ansi.as_bytes());
        Self {
            ansi,
            cols,
            rows,
            sha256,
            format_version,
            captured_at_ms,
            truncated,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRecord {
    pub id: u64,
    pub title: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    /// Profile used to spawn this pane. `None` means the session default.
    pub profile: Option<String>,
    pub cwd: Option<String>,
    pub snapshot: Option<PaneSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived: bool,
    pub cold_recovered_at_ms: Option<i64>,
    pub layout: SessionLayout,
    pub panes: Vec<PaneRecord>,
}

impl SessionData {
    pub fn new(name: impl Into<String>, layout: SessionLayout, panes: Vec<PaneRecord>) -> Self {
        let now = now_ms();
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            created_at_ms: now,
            updated_at_ms: now,
            archived: false,
            cold_recovered_at_ms: None,
            layout,
            panes,
        }
    }

    pub fn validate(&mut self) -> Result<(), SessionError> {
        validate_session_name(&self.name).map_err(|_| {
            SessionError::Invalid(
                "session name must be 1..=64 ASCII letters, digits, '-', '_' or '.'".into(),
            )
        })?;
        if self.id.trim().is_empty() || self.id.len() > 128 {
            return Err(SessionError::Invalid("session id is invalid".into()));
        }
        if self.layout.tabs.len() > 64 {
            return Err(SessionError::Invalid("session has too many tabs".into()));
        }
        self.layout
            .validate()
            .map_err(|error| SessionError::Invalid(format!("layout is invalid: {error}")))?;
        let layout_panes: Vec<u64> = self.layout.all_panes_including_minimized();
        let mut seen = HashSet::new();
        for pane in &layout_panes {
            if !seen.insert(*pane) {
                return Err(SessionError::Invalid(format!(
                    "pane {pane} appears more than once"
                )));
            }
        }
        let record_ids: BTreeSet<u64> = self.panes.iter().map(|p| p.id).collect();
        if record_ids.len() != self.panes.len() {
            return Err(SessionError::Invalid("duplicate pane records".into()));
        }
        let layout_ids: BTreeSet<u64> = layout_panes.iter().copied().collect();
        if record_ids != layout_ids {
            return Err(SessionError::Invalid(
                "pane records do not match panes in the layout".into(),
            ));
        }

        let mut snapshot_bytes = 0usize;
        for pane in &mut self.panes {
            if pane.title.chars().count() > 128 {
                return Err(SessionError::Invalid("pane title is too long".into()));
            }
            if pane.title.chars().any(|c| c.is_control()) {
                return Err(SessionError::Invalid(
                    "pane title contains control characters".into(),
                ));
            }
            if let Some(profile) = &pane.profile {
                if profile.is_empty()
                    || profile.len() > 64
                    || !profile
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    return Err(SessionError::Invalid(format!(
                        "pane {} has an invalid profile name",
                        pane.id
                    )));
                }
            }
            if let Some(cwd) = &pane.cwd {
                if cwd.len() > MAX_CWD_CHARS || cwd.contains('\0') {
                    return Err(SessionError::Invalid(format!(
                        "pane {} has an invalid working directory",
                        pane.id
                    )));
                }
            }
            if let Some(snapshot) = &pane.snapshot {
                snapshot.validate()?;
                snapshot_bytes = snapshot_bytes.saturating_add(snapshot.ansi.len());
            }
        }
        if snapshot_bytes > MAX_EXPORT_BYTES {
            return Err(SessionError::Invalid(
                "combined pane snapshots are too large".into(),
            ));
        }
        for tab in &mut self.layout.tabs {
            tab.title = fact_protocol::sanitize_title(&tab.title);
        }
        Ok(())
    }

    pub fn summary(&self, live: bool) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            archived: self.archived,
            live,
            pane_count: self.panes.len(),
            tab_count: self.layout.tabs.len(),
            cold_recovered_at_ms: self.cold_recovered_at_ms,
        }
    }

    pub fn pane(&self, id: u64) -> Option<&PaneRecord> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub fn pane_mut(&mut self, id: u64) -> Option<&mut PaneRecord> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived: bool,
    pub cold_recovered_at_ms: Option<i64>,
    pub pane_count: usize,
    pub tab_count: usize,
}

impl SessionMeta {
    pub fn to_summary(&self, live: bool) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            archived: self.archived,
            live,
            pane_count: self.pane_count,
            tab_count: self.tab_count,
            cold_recovered_at_ms: self.cold_recovered_at_ms,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ExportEnvelope {
    format: String,
    version: u32,
    exported_at_ms: i64,
    session: SessionData,
}

pub struct SessionStore {
    conn: Connection,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let store = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, SessionError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn, path: None };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn configure(&self) -> Result<(), SessionError> {
        // WAL gives daemon readers predictable latency. The daemon still owns
        // the only writer connection.
        if self.path.is_some() {
            self.conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        self.conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }

    fn migrate(&self) -> Result<(), SessionError> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(SessionError::Schema(version));
        }
        if version == 0 {
            self.conn.execute_batch(
                r#"
                BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0,1)),
                    layout_json TEXT NOT NULL,
                    cold_recovered_at_ms INTEGER
                );
                CREATE TABLE IF NOT EXISTS panes (
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    pane_id INTEGER NOT NULL,
                    title TEXT NOT NULL,
                    pid INTEGER NOT NULL DEFAULT 0,
                    exit_code INTEGER,
                    profile TEXT,
                    cwd TEXT,
                    snapshot_ansi TEXT,
                    snapshot_cols INTEGER,
                    snapshot_rows INTEGER,
                    snapshot_sha256 TEXT,
                    snapshot_format INTEGER,
                    snapshot_captured_at_ms INTEGER,
                    snapshot_truncated INTEGER,
                    PRIMARY KEY (session_id, pane_id)
                );
                CREATE INDEX IF NOT EXISTS panes_session_idx ON panes(session_id);
                CREATE INDEX IF NOT EXISTS sessions_updated_idx ON sessions(updated_at_ms DESC);
                PRAGMA user_version = 1;
                COMMIT;
                "#,
            )?;
        }
        Ok(())
    }

    pub fn save(&mut self, session: &SessionData) -> Result<(), SessionError> {
        let mut session = session.clone();
        session.validate()?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE name = ?1 COLLATE NOCASE OR id = ?2",
            params![session.name, session.id],
        )?;
        insert_session(&tx, &session)?;
        tx.commit()?;
        Ok(())
    }

    pub fn insert_new(&mut self, session: &SessionData) -> Result<(), SessionError> {
        let mut session = session.clone();
        session.validate()?;
        if self.exists(&session.name)? {
            return Err(SessionError::AlreadyExists(session.name));
        }
        let tx = self.conn.transaction()?;
        insert_session(&tx, &session)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<SessionData, SessionError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, created_at_ms, updated_at_ms, archived,
                        layout_json, cold_recovered_at_ms
                 FROM sessions WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |row| {
                    Ok(StoredSessionRow {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at_ms: row.get(2)?,
                        updated_at_ms: row.get(3)?,
                        archived: row.get::<_, i64>(4)? != 0,
                        layout_json: row.get(5)?,
                        cold_recovered_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?;
        let row = row.ok_or_else(|| SessionError::NotFound(name.to_string()))?;
        self.load_row(row)
    }

    pub fn load_by_id(&self, id: &str) -> Result<SessionData, SessionError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, name, created_at_ms, updated_at_ms, archived,
                        layout_json, cold_recovered_at_ms
                 FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    Ok(StoredSessionRow {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at_ms: row.get(2)?,
                        updated_at_ms: row.get(3)?,
                        archived: row.get::<_, i64>(4)? != 0,
                        layout_json: row.get(5)?,
                        cold_recovered_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?;
        let row = row.ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        self.load_row(row)
    }

    fn load_row(&self, row: StoredSessionRow) -> Result<SessionData, SessionError> {
        let layout: SessionLayout = serde_json::from_str(&row.layout_json)
            .map_err(|error| SessionError::Corrupt(format!("layout JSON: {error}")))?;
        let mut panes = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT pane_id, title, pid, exit_code, profile, cwd,
                    snapshot_ansi, snapshot_cols, snapshot_rows, snapshot_sha256,
                    snapshot_format, snapshot_captured_at_ms, snapshot_truncated
             FROM panes WHERE session_id = ?1 ORDER BY pane_id",
        )?;
        let rows = statement.query_map(params![row.id], |record| {
            let snapshot_ansi: Option<String> = record.get(6)?;
            let snapshot = snapshot_ansi.map(|ansi| PaneSnapshot {
                ansi,
                cols: record.get::<_, Option<u16>>(7).unwrap_or(None).unwrap_or(0),
                rows: record.get::<_, Option<u16>>(8).unwrap_or(None).unwrap_or(0),
                sha256: record
                    .get::<_, Option<String>>(9)
                    .unwrap_or(None)
                    .unwrap_or_default(),
                format_version: record
                    .get::<_, Option<u32>>(10)
                    .unwrap_or(None)
                    .unwrap_or(0),
                captured_at_ms: record
                    .get::<_, Option<i64>>(11)
                    .unwrap_or(None)
                    .unwrap_or(0),
                truncated: record
                    .get::<_, Option<i64>>(12)
                    .unwrap_or(None)
                    .unwrap_or(0)
                    != 0,
            });
            Ok(PaneRecord {
                id: record.get::<_, i64>(0)? as u64,
                title: record.get(1)?,
                pid: record.get(2)?,
                exit_code: record.get(3)?,
                profile: record.get(4)?,
                cwd: record.get(5)?,
                snapshot,
            })
        })?;
        for pane in rows {
            panes.push(pane?);
        }
        drop(statement);
        let mut session = SessionData {
            id: row.id,
            name: row.name,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            archived: row.archived,
            cold_recovered_at_ms: row.cold_recovered_at_ms,
            layout,
            panes,
        };
        session.layout.sanitize();
        session.validate()?;
        Ok(session)
    }

    pub fn exists(&self, name: &str) -> Result<bool, SessionError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE name = ?1 COLLATE NOCASE",
            params![name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn count(&self) -> Result<usize, SessionError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn list(&self, include_archived: bool) -> Result<Vec<SessionMeta>, SessionError> {
        let mut statement = self.conn.prepare(
            "SELECT id, name, created_at_ms, updated_at_ms, archived,
                    layout_json, cold_recovered_at_ms
             FROM sessions
             WHERE (?1 = 1 OR archived = 0)
             ORDER BY archived ASC, updated_at_ms DESC, name ASC",
        )?;
        let rows = statement.query_map(params![if include_archived { 1 } else { 0 }], |row| {
            Ok(StoredSessionRow {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at_ms: row.get(2)?,
                updated_at_ms: row.get(3)?,
                archived: row.get::<_, i64>(4)? != 0,
                layout_json: row.get(5)?,
                cold_recovered_at_ms: row.get(6)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            let row = row?;
            let layout: SessionLayout = serde_json::from_str(&row.layout_json)
                .map_err(|error| SessionError::Corrupt(format!("layout JSON: {error}")))?;
            sessions.push(SessionMeta {
                id: row.id,
                name: row.name,
                created_at_ms: row.created_at_ms,
                updated_at_ms: row.updated_at_ms,
                archived: row.archived,
                cold_recovered_at_ms: row.cold_recovered_at_ms,
                pane_count: layout.all_panes_including_minimized().len(),
                tab_count: layout.tabs.len(),
            });
        }
        Ok(sessions)
    }

    /// Most recently updated non-archived session, preferring live sessions in
    /// the daemon's own selection logic.
    pub fn latest(&self, include_archived: bool) -> Result<Option<SessionData>, SessionError> {
        let name: Option<String> = self
            .conn
            .query_row(
                "SELECT name FROM sessions
                 WHERE (?1 = 1 OR archived = 0)
                 ORDER BY updated_at_ms DESC, rowid DESC LIMIT 1",
                params![if include_archived { 1 } else { 0 }],
                |row| row.get(0),
            )
            .optional()?;
        match name {
            Some(name) => self.load(&name).map(Some),
            None => Ok(None),
        }
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), SessionError> {
        validate_session_name(new).map_err(|_| {
            SessionError::Invalid(
                "session name must be 1..=64 ASCII letters, digits, '-', '_' or '.'".into(),
            )
        })?;
        if old.eq_ignore_ascii_case(new) {
            // SQLite's NOCASE collation considers these equal, so an UPDATE is
            // still needed to persist a pure case change.
            if old != new {
                let changed = self.conn.execute(
                    "UPDATE sessions SET name = ?1, updated_at_ms = ?2
                     WHERE name = ?3 COLLATE NOCASE",
                    params![new, now_ms(), old],
                )?;
                if changed == 0 {
                    return Err(SessionError::NotFound(old.to_string()));
                }
            }
            return Ok(());
        }
        if self.exists(new)? {
            return Err(SessionError::AlreadyExists(new.to_string()));
        }
        let changed = self.conn.execute(
            "UPDATE sessions SET name = ?1, updated_at_ms = ?2
             WHERE name = ?3 COLLATE NOCASE",
            params![new, now_ms(), old],
        )?;
        if changed == 0 {
            return Err(SessionError::NotFound(old.to_string()));
        }
        Ok(())
    }

    pub fn set_archived(&mut self, name: &str, archived: bool) -> Result<(), SessionError> {
        let changed = self.conn.execute(
            "UPDATE sessions SET archived = ?1, updated_at_ms = ?2
             WHERE name = ?3 COLLATE NOCASE",
            params![if archived { 1 } else { 0 }, now_ms(), name],
        )?;
        if changed == 0 {
            return Err(SessionError::NotFound(name.to_string()));
        }
        Ok(())
    }

    pub fn touch(&mut self, name: &str) -> Result<(), SessionError> {
        self.conn.execute(
            "UPDATE sessions SET updated_at_ms = ?1 WHERE name = ?2 COLLATE NOCASE",
            params![now_ms(), name],
        )?;
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), SessionError> {
        let changed = self.conn.execute(
            "DELETE FROM sessions WHERE name = ?1 COLLATE NOCASE",
            params![name],
        )?;
        if changed == 0 {
            return Err(SessionError::NotFound(name.to_string()));
        }
        Ok(())
    }

    pub fn export_json(&self, name: &str) -> Result<(String, String), SessionError> {
        let session = self.load(name)?;
        let envelope = ExportEnvelope {
            format: EXPORT_FORMAT.to_string(),
            version: EXPORT_VERSION,
            exported_at_ms: now_ms(),
            session,
        };
        let json = serde_json::to_string_pretty(&envelope).map_err(|error| {
            SessionError::Corrupt(format!("could not serialize export: {error}"))
        })?;
        if json.len() > MAX_EXPORT_BYTES {
            return Err(SessionError::TooLarge { size: json.len() });
        }
        let filename = format!("fact-{name}-{}.json", now_ms());
        Ok((filename, json))
    }

    pub fn import_json(
        &mut self,
        json: &str,
        name_override: Option<&str>,
        overwrite: bool,
    ) -> Result<SessionData, SessionError> {
        if json.len() > MAX_EXPORT_BYTES {
            return Err(SessionError::TooLarge { size: json.len() });
        }
        let mut envelope: ExportEnvelope = serde_json::from_str(json)
            .map_err(|error| SessionError::Corrupt(format!("export JSON: {error}")))?;
        if envelope.format != EXPORT_FORMAT {
            return Err(SessionError::Corrupt(format!(
                "unexpected export format '{}'",
                envelope.format
            )));
        }
        if envelope.version != EXPORT_VERSION {
            return Err(SessionError::Corrupt(format!(
                "unsupported export version {}",
                envelope.version
            )));
        }
        if let Some(name) = name_override {
            envelope.session.name = name.to_string();
        }
        envelope.session.validate()?;
        if !overwrite && self.exists(&envelope.session.name)? {
            return Err(SessionError::AlreadyExists(envelope.session.name));
        }
        // Imports are copies, never replays of another daemon's process state.
        envelope.session.id = Uuid::new_v4().to_string();
        envelope.session.created_at_ms = now_ms();
        envelope.session.updated_at_ms = envelope.session.created_at_ms;
        envelope.session.archived = false;
        envelope.session.cold_recovered_at_ms = None;
        for pane in &mut envelope.session.panes {
            pane.pid = 0;
            pane.exit_code = None;
        }
        envelope.session.validate()?;
        if overwrite {
            self.save(&envelope.session)?;
        } else {
            self.insert_new(&envelope.session)?;
        }
        Ok(envelope.session)
    }
}

struct StoredSessionRow {
    id: String,
    name: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    archived: bool,
    layout_json: String,
    cold_recovered_at_ms: Option<i64>,
}

fn insert_session(tx: &Transaction<'_>, session: &SessionData) -> Result<(), SessionError> {
    let layout_json = serde_json::to_string(&session.layout)
        .map_err(|error| SessionError::Invalid(format!("layout serialization: {error}")))?;
    tx.execute(
        "INSERT INTO sessions
         (id, name, created_at_ms, updated_at_ms, archived, layout_json, cold_recovered_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session.id,
            session.name,
            session.created_at_ms,
            session.updated_at_ms,
            if session.archived { 1 } else { 0 },
            layout_json,
            session.cold_recovered_at_ms
        ],
    )?;
    for pane in &session.panes {
        let snapshot = pane.snapshot.as_ref();
        tx.execute(
            "INSERT INTO panes
             (session_id, pane_id, title, pid, exit_code, profile, cwd,
              snapshot_ansi, snapshot_cols, snapshot_rows, snapshot_sha256,
              snapshot_format, snapshot_captured_at_ms, snapshot_truncated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                session.id,
                pane.id as i64,
                pane.title,
                pane.pid,
                pane.exit_code,
                pane.profile,
                pane.cwd,
                snapshot.map(|s| s.ansi.as_str()),
                snapshot.map(|s| s.cols),
                snapshot.map(|s| s.rows),
                snapshot.map(|s| s.sha256.as_str()),
                snapshot.map(|s| s.format_version),
                snapshot.map(|s| s.captured_at_ms),
                snapshot.map(|s| if s.truncated { 1i64 } else { 0i64 }),
            ],
        )?;
    }
    Ok(())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build a simple tab title from a pane title.
pub fn title_for_tab(title: &str) -> String {
    fact_protocol::sanitize_title(title)
}

/// Convenience for deterministic tests and future import tooling.
pub fn empty_tab(id: u32) -> TabLayout {
    TabLayout {
        id,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fact_layout::{LayoutNode, TabLayout};

    fn sample_snapshot() -> PaneSnapshot {
        PaneSnapshot::new("hello\r\n".to_string(), 40, 10, 1, now_ms(), false)
    }

    fn sample_session() -> SessionData {
        let layout = SessionLayout::new(1, &[10, 11]);
        SessionData::new(
            "dev",
            layout,
            vec![
                PaneRecord {
                    id: 10,
                    title: "PowerShell".into(),
                    pid: 1234,
                    exit_code: None,
                    profile: Some("auto".into()),
                    cwd: Some(r"C:\Users\test".into()),
                    snapshot: Some(sample_snapshot()),
                },
                PaneRecord {
                    id: 11,
                    title: "cmd".into(),
                    pid: 1235,
                    exit_code: Some(0),
                    profile: None,
                    cwd: None,
                    snapshot: None,
                },
            ],
        )
    }

    #[test]
    fn save_load_round_trip() {
        let mut store = SessionStore::open_in_memory().unwrap();
        let session = sample_session();
        store.save(&session).unwrap();
        let loaded = store.load("dev").unwrap();
        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.panes[0].snapshot.as_ref().unwrap().ansi, "hello\r\n");
        assert_eq!(store.list(false).unwrap().len(), 1);
        assert_eq!(store.list(false).unwrap()[0].pane_count, 2);
    }

    #[test]
    fn case_insensitive_names_and_rename() {
        let mut store = SessionStore::open_in_memory().unwrap();
        store.save(&sample_session()).unwrap();
        assert!(store.exists("DEV").unwrap());
        assert!(matches!(store.load("DEV").unwrap().name.as_str(), "dev"));
        store.rename("DEV", "work").unwrap();
        assert!(store.load("work").is_ok());
        assert!(store.rename("work", "WORK2").is_ok());
        assert!(store.load("WORK2").is_ok());
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let mut store = SessionStore::open_in_memory().unwrap();
        let session = sample_session();
        store.save(&session).unwrap();
        store
            .conn
            .execute(
                "UPDATE panes SET snapshot_ansi = 'tampered' WHERE session_id = ?1 AND pane_id = 10",
                params![session.id],
            )
            .unwrap();
        assert!(store.load("dev").is_err());
    }

    #[test]
    fn rejects_pane_layout_mismatch() {
        let mut store = SessionStore::open_in_memory().unwrap();
        let mut session = sample_session();
        session.panes.pop();
        assert!(store.save(&session).is_err());
    }

    #[test]
    fn export_import_is_a_copy() {
        let mut store = SessionStore::open_in_memory().unwrap();
        store.save(&sample_session()).unwrap();
        let (filename, json) = store.export_json("dev").unwrap();
        assert!(filename.starts_with("fact-dev-"));
        assert!(json.contains(EXPORT_FORMAT));
        let imported = store.import_json(&json, Some("copy"), false).unwrap();
        assert_eq!(imported.name, "copy");
        assert_ne!(imported.id, "dev");
        assert!(imported.panes.iter().all(|pane| pane.pid == 0));
        assert!(store.load("copy").is_ok());
        assert!(matches!(
            store.import_json(&json, Some("copy"), false),
            Err(SessionError::AlreadyExists(_))
        ));
    }

    #[test]
    fn import_rejects_unknown_format() {
        let mut store = SessionStore::open_in_memory().unwrap();
        assert!(store.import_json("{}", None, false).is_err());
    }

    #[test]
    fn archive_and_delete() {
        let mut store = SessionStore::open_in_memory().unwrap();
        store.save(&sample_session()).unwrap();
        store.set_archived("dev", true).unwrap();
        assert!(store.list(false).unwrap().is_empty());
        assert_eq!(store.list(true).unwrap().len(), 1);
        store.delete("dev").unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn layout_with_minimized_panes_is_valid() {
        let mut tab = TabLayout {
            id: 1,
            root: Some(LayoutNode::Leaf { pane: 1 }),
            focus: Some(1),
            minimized: vec![2],
            ..Default::default()
        };
        tab.title = "one".into();
        let layout = SessionLayout {
            tabs: vec![tab],
            active_tab: 1,
        };
        let mut session = SessionData::new(
            "minimized",
            layout,
            vec![
                PaneRecord {
                    id: 1,
                    title: "one".into(),
                    pid: 1,
                    exit_code: None,
                    profile: None,
                    cwd: None,
                    snapshot: None,
                },
                PaneRecord {
                    id: 2,
                    title: "two".into(),
                    pid: 2,
                    exit_code: None,
                    profile: None,
                    cwd: None,
                    snapshot: None,
                },
            ],
        );
        session.validate().unwrap();
    }
}
