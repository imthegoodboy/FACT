//! Daemon-owned session state: real panes, layouts, snapshots and frame fan-out.
//!
//! All mutation happens on one owner thread. Control connections submit
//! commands through a channel, which keeps PTY/output handling and SQLite
//! writes serialized without a global lock.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use fact_config::{Config, Paths};
use fact_layout::{
    LayoutNode, LayoutOptions, SessionLayout, SplitDirection as LayoutSplitDirection,
};
use fact_protocol::{
    sanitize_title, validate_session_name, AttachInfo, Command, CursorDto, DaemonStats, ErrorCode,
    FocusDirection, Frame, PaneSummary, PaneView, ProtocolError, ResponsePayload, SearchHit,
    SearchResult, SessionSummary, SplitHandle, SplitOrientation, TabView, Viewport, MAX_GRID_PANES,
    MAX_INPUT_CHUNK, MAX_PASTE_BYTES, MAX_SESSIONS,
};
use fact_pty::{PtyProcess, SpawnOptions};
use fact_session::{
    now_ms, PaneRecord, PaneSnapshot, SessionData, SessionError, SessionMeta, SessionStore,
};
use fact_term::{TerminalEventSink, TerminalModel};

const DEFAULT_VIEWPORT_COLS: u16 = 120;
const DEFAULT_VIEWPORT_ROWS: u16 = 35;
const STREAM_TOKEN_TTL: Duration = Duration::from_secs(120);
const OUTPUT_CHUNKS_PER_TICK: usize = 64;
const MAX_PANES_PER_SESSION: usize = 256;

struct PaneSpec<'a> {
    profile: Option<&'a str>,
    cwd: Option<&'a str>,
    cols: u16,
    rows: u16,
    snapshot: Option<&'a PaneSnapshot>,
    title_hint: &'a str,
    cold_recovery: bool,
}

pub(crate) struct SessionManager {
    paths: Paths,
    config: Config,
    store: SessionStore,
    sessions: HashMap<String, LiveSession>,
    subscribers: Vec<Subscriber>,
    stream_grants: HashMap<String, StreamGrant>,
    next_pane_id: u64,
    next_frame_seq: u64,
    shutdown: bool,
    started: Instant,
}

struct LiveSession {
    data: SessionData,
    panes: HashMap<u64, LivePane>,
    viewport_cols: u16,
    viewport_rows: u16,
    layout_epoch: u64,
    metadata_epoch: u64,
    detach_seq: u64,
    cold_recovered: bool,
    recovery_notes: Vec<String>,
    last_persist: Instant,
    last_snapshot: Instant,
    last_active_ms: i64,
}

struct LivePane {
    pty: PtyProcess,
    term: TerminalModel,
    responses: Receiver<Vec<u8>>,
    version: u64,
    default_title: String,
    display_title: String,
    last_bell: u64,
    exit_code: Option<i32>,
    profile: Option<String>,
    cwd: Option<String>,
}

struct Subscriber {
    tx: SyncSender<fact_protocol::ServerMessage>,
    session_key: String,
    sent_versions: HashMap<u64, u64>,
    layout_epoch: u64,
    metadata_epoch: u64,
}

struct StreamGrant {
    session_key: String,
    created: Instant,
}

struct BuiltFrame {
    frame: Frame,
    versions: Vec<(u64, u64)>,
    layout_epoch: u64,
    metadata_epoch: u64,
}

impl SessionManager {
    pub(crate) fn new(paths: Paths, config: Config) -> Result<Self, SessionError> {
        let store = SessionStore::open(&paths.db_file)?;
        Ok(Self {
            paths,
            config,
            store,
            sessions: HashMap::new(),
            subscribers: Vec::new(),
            stream_grants: HashMap::new(),
            next_pane_id: 1,
            next_frame_seq: 0,
            shutdown: false,
            started: Instant::now(),
        })
    }

    pub(crate) fn handle(
        &mut self,
        command: Command,
        bound_session: Option<&str>,
    ) -> Result<ResponsePayload, ProtocolError> {
        match command {
            Command::Ping => Ok(ResponsePayload::Pong {
                daemon_pid: std::process::id(),
                uptime_ms: self.started.elapsed().as_millis() as u64,
                live_sessions: self.sessions.len(),
            }),
            Command::Stats => Ok(ResponsePayload::Stats(self.stats())),
            Command::ListSessions { include_archived } => self
                .list_sessions(include_archived)
                .map(ResponsePayload::Sessions),
            Command::ResumeSession {
                name,
                grid,
                profile,
                cwd,
            } => self
                .resume_session(name, grid, profile, cwd)
                .map(|info| ResponsePayload::Attached(Box::new(info))),
            Command::Attach { name } => self
                .attach_session(&name)
                .map(|info| ResponsePayload::Attached(Box::new(info))),
            Command::RenameSession { name, new_name } => self
                .rename_session(&name, &new_name)
                .map(ResponsePayload::Session),
            Command::SaveSession { name } => {
                let key = self.session_key(&name)?;
                self.snapshot_session(&key)?;
                self.persist_session(&key)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ArchiveSession { name } => self
                .archive_session(&name, true)
                .map(ResponsePayload::Session),
            Command::UnarchiveSession { name } => self
                .archive_session(&name, false)
                .map(ResponsePayload::Session),
            Command::DeleteSession { name, force } => {
                self.delete_session(&name, force)?;
                Ok(ResponsePayload::Ack)
            }
            Command::TerminateSession { name } => {
                self.terminate_session(&name)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ExportSession { name } => {
                if let Some(key) = self.session_key_opt(&name) {
                    self.snapshot_session(&key)?;
                    self.persist_session(&key)?;
                } else if !self.store.exists(&name).map_err(map_session_error)? {
                    return Err(ProtocolError::not_found(format!("session '{name}'")));
                }
                let (filename, json) = self.store.export_json(&name).map_err(map_session_error)?;
                Ok(ResponsePayload::Export { filename, json })
            }
            Command::ImportSession {
                json,
                name,
                overwrite,
            } => {
                if json.len() > fact_session::MAX_EXPORT_BYTES {
                    return Err(ProtocolError::invalid("session export is too large"));
                }
                if let Some(name) = name.as_deref() {
                    validate_session_name(name)?;
                    if self.session_key_opt(name).is_some() {
                        return Err(ProtocolError::new(
                            ErrorCode::Conflict,
                            "cannot import over a live session",
                        ));
                    }
                }
                let imported = self
                    .store
                    .import_json(&json, name.as_deref(), overwrite)
                    .map_err(map_session_error)?;
                Ok(ResponsePayload::Imported(imported.summary(false)))
            }
            Command::DetachClients { name } => {
                self.detach_clients(name.as_deref())?;
                Ok(ResponsePayload::Ack)
            }
            Command::ShutdownDaemon => {
                self.shutdown = true;
                Ok(ResponsePayload::Ack)
            }
            Command::Input { pane, data } => {
                self.write_input(pane, &data, bound_session)?;
                Ok(ResponsePayload::Ack)
            }
            Command::Paste { pane, text } => {
                self.paste(pane, &text, bound_session)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ResizeClient { cols, rows } => {
                let key = self.require_bound(bound_session)?;
                self.resize_viewport(&key, cols, rows)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SplitPane { pane, direction } => {
                let key = self.require_bound(bound_session)?;
                self.split_pane(&key, pane, direction)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ClosePane { pane } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.close_pane(&key, pane)?;
                Ok(ResponsePayload::Ack)
            }
            Command::RestartPane { pane } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.restart_pane(&key, pane)?;
                Ok(ResponsePayload::Ack)
            }
            Command::FocusPane { pane } => {
                let key = self.require_bound(bound_session)?;
                self.focus_pane(&key, pane)?;
                Ok(ResponsePayload::Ack)
            }
            Command::FocusDirection { direction } => {
                let key = self.require_bound(bound_session)?;
                self.focus_direction(&key, direction)?;
                Ok(ResponsePayload::Ack)
            }
            Command::NextPane => {
                let key = self.require_bound(bound_session)?;
                self.cycle_focus(&key, 1)?;
                Ok(ResponsePayload::Ack)
            }
            Command::PrevPane => {
                let key = self.require_bound(bound_session)?;
                self.cycle_focus(&key, -1)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ZoomPane { zoomed } => {
                let key = self.require_bound(bound_session)?;
                self.zoom(&key, zoomed)?;
                Ok(ResponsePayload::Ack)
            }
            Command::MinimizePane { pane } => {
                let key = self.require_bound(bound_session)?;
                self.minimize_pane(&key, pane)?;
                Ok(ResponsePayload::Ack)
            }
            Command::RestorePanes => {
                let key = self.require_bound(bound_session)?;
                self.restore_panes(&key)?;
                Ok(ResponsePayload::Ack)
            }
            Command::NewTab { profile, cwd } => {
                let key = self.require_bound(bound_session)?;
                self.new_tab(&key, profile, cwd)?;
                Ok(ResponsePayload::Ack)
            }
            Command::CloseTab { tab } => {
                let key = self.require_bound(bound_session)?;
                self.close_tab(&key, tab)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SelectTab { tab } => {
                let key = self.require_bound(bound_session)?;
                self.select_tab(&key, tab)?;
                Ok(ResponsePayload::Ack)
            }
            Command::RenameTab { tab, title } => {
                let key = self.require_bound(bound_session)?;
                self.rename_tab(&key, tab, &title)?;
                Ok(ResponsePayload::Ack)
            }
            Command::NextTab => {
                let key = self.require_bound(bound_session)?;
                self.change_tab(&key, 1)?;
                Ok(ResponsePayload::Ack)
            }
            Command::PrevTab => {
                let key = self.require_bound(bound_session)?;
                self.change_tab(&key, -1)?;
                Ok(ResponsePayload::Ack)
            }
            Command::MovePaneToTab { pane, target_tab } => {
                let key = self.require_bound(bound_session)?;
                self.move_pane(&key, pane, target_tab)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SwapPanes { a, b } => {
                let key = self.require_bound(bound_session)?;
                self.swap_panes(&key, a, b)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ScrollPane { pane, delta } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.scroll(&key, pane, delta)?;
                Ok(ResponsePayload::Ack)
            }
            Command::ScrollToLine { pane, line } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.scroll_to_line(&key, pane, line)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SetScrollOffset { pane, offset } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.set_scroll_offset(&key, pane, offset)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SearchPane {
                pane,
                query,
                case_sensitive,
                limit,
            } => {
                let key = self.pane_owner(pane, bound_session)?;
                self.search(&key, pane, &query, case_sensitive, limit)
                    .map(ResponsePayload::Search)
            }
            Command::DragSplit { path, ratio } => {
                let key = self.require_bound(bound_session)?;
                self.drag_split(&key, &path, ratio)?;
                Ok(ResponsePayload::Ack)
            }
        }
    }

    pub(crate) fn tick(&mut self) {
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            let mut meta_delta = 0u64;
            let snapshot_due = {
                let Some(session) = self.sessions.get(&key) else {
                    continue;
                };
                session.last_snapshot.elapsed()
                    >= Duration::from_secs(self.config.snapshot_interval_secs.max(1))
            };
            if let Some(session) = self.sessions.get_mut(&key) {
                for pane in session.panes.values_mut() {
                    let mut dirty = false;
                    for _ in 0..OUTPUT_CHUNKS_PER_TICK {
                        match pane.pty.try_read() {
                            Some(bytes) => {
                                pane.term.feed(&bytes);
                                dirty = true;
                            }
                            None => break,
                        }
                    }
                    if dirty {
                        pane.version = pane.version.wrapping_add(1);
                    }
                    while let Ok(response) = pane.responses.try_recv() {
                        if pane.pty.write(&response).is_err() {
                            break;
                        }
                    }
                    if pane.exit_code.is_none() {
                        if let Some(exit) = pane.pty.exit() {
                            pane.exit_code = Some(exit.code);
                            pane.term.feed(
                                format!(
                                    "\r\n\x1b[90m[FACT] process exited with code {}\x1b[0m\r\n",
                                    exit.code
                                )
                                .as_bytes(),
                            );
                            pane.version = pane.version.wrapping_add(1);
                            meta_delta += 1;
                        }
                    }
                    let bell = pane.term.bell_seq();
                    if bell != pane.last_bell {
                        pane.last_bell = bell;
                        meta_delta += 1;
                    }
                    let title = if pane.term.title().trim().is_empty() {
                        pane.default_title.clone()
                    } else {
                        fact_protocol::friendly_title(&pane.term.title())
                    };
                    if title != pane.display_title {
                        pane.display_title = title;
                        meta_delta += 1;
                    }
                }
                if meta_delta > 0 {
                    session.metadata_epoch = session.metadata_epoch.wrapping_add(meta_delta);
                }
                let title_changes = {
                    let panes = &session.panes;
                    let mut changes = 0u64;
                    for tab in &mut session.data.layout.tabs {
                        if tab.custom_title {
                            continue;
                        }
                        let Some(focus) =
                            tab.focus.or_else(|| tab.root.as_ref().and_then(first_pane))
                        else {
                            continue;
                        };
                        let Some(pane) = panes.get(&focus) else {
                            continue;
                        };
                        if tab.title != pane.display_title {
                            tab.title = pane.display_title.clone();
                            changes += 1;
                        }
                    }
                    changes
                };
                if title_changes > 0 {
                    session.metadata_epoch =
                        session.metadata_epoch.wrapping_add(title_changes.min(1));
                }
            }
            if snapshot_due && self.snapshot_session(&key).is_ok() {
                let _ = self.persist_session(&key);
                continue;
            }
            let persist_due = self.sessions.get(&key).is_some_and(|session| {
                session.last_persist.elapsed()
                    >= Duration::from_secs(self.config.persist_interval_secs.max(1))
            });
            if persist_due {
                let _ = self.persist_session(&key);
            }
        }
        self.stream_grants
            .retain(|_, grant| grant.created.elapsed() < STREAM_TOKEN_TTL);
        self.pump_frames();
    }

    pub(crate) fn subscribe(
        &mut self,
        token: &str,
        tx: SyncSender<fact_protocol::ServerMessage>,
    ) -> Result<(), ProtocolError> {
        let grant = self.stream_grants.remove(token).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::Unauthorized,
                "event stream token is invalid, expired or already used",
            )
        })?;
        if grant.created.elapsed() >= STREAM_TOKEN_TTL {
            return Err(ProtocolError::new(
                ErrorCode::Unauthorized,
                "event stream token has expired; reattach to the session",
            ));
        }
        if !self.sessions.contains_key(&grant.session_key) {
            return Err(ProtocolError::not_found(
                "the subscribed session is no longer running",
            ));
        }
        self.subscribers.push(Subscriber {
            tx,
            session_key: grant.session_key,
            sent_versions: HashMap::new(),
            layout_epoch: 0,
            metadata_epoch: 0,
        });
        self.pump_frames();
        Ok(())
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown
    }

    pub(crate) fn shutdown(&mut self) {
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            let _ = self.snapshot_session(&key);
            let _ = self.persist_session(&key);
        }
        for (_, session) in self.sessions.drain() {
            for (_, pane) in session.panes {
                pane.pty.kill();
                drop(pane);
            }
        }
        for subscriber in self.subscribers.drain(..) {
            let _ = subscriber
                .tx
                .try_send(fact_protocol::ServerMessage::SessionEnded {
                    reason: "daemon is shutting down".into(),
                });
        }
        self.stream_grants.clear();
    }

    pub(crate) fn database_path(&self) -> String {
        self.paths.db_file.display().to_string()
    }

    fn stats(&self) -> DaemonStats {
        DaemonStats {
            daemon_pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: fact_protocol::PROTOCOL_VERSION,
            uptime_ms: self.started.elapsed().as_millis() as u64,
            live_sessions: self.sessions.len(),
            live_panes: self.sessions.values().map(|s| s.panes.len()).sum(),
            client_count: self.subscribers.len(),
            database_path: self.database_path(),
            memory_bytes: None,
            cpu_time_ms: None,
        }
    }

    fn resume_session(
        &mut self,
        name: Option<String>,
        grid: Option<u8>,
        profile: Option<String>,
        cwd: Option<String>,
    ) -> Result<AttachInfo, ProtocolError> {
        let name = match name {
            Some(name) => {
                validate_session_name(&name)?;
                name
            }
            None => self.pick_resume_name()?,
        };
        let key = key_of(&name);
        if self.sessions.contains_key(&key) {
            return self.attach_live(&key);
        }

        if self.store.exists(&name).map_err(map_session_error)? {
            let data = self.store.load(&name).map_err(map_session_error)?;
            if data.archived {
                return Err(ProtocolError::new(
                    ErrorCode::Conflict,
                    format!("session '{name}' is archived; unarchive it before resuming"),
                ));
            }
            self.cold_recover(data)?;
            let key = key_of(&name);
            return self.attach_live(&key);
        }

        self.create_session(name, grid.unwrap_or(1), profile.as_deref(), cwd.as_deref())
    }

    fn pick_resume_name(&self) -> Result<String, ProtocolError> {
        if let Some((_, session)) = self
            .sessions
            .iter()
            .max_by_key(|(_, session)| session.last_active_ms)
        {
            return Ok(session.data.name.clone());
        }
        match self.store.latest(false) {
            Ok(Some(session)) => Ok(session.name),
            Ok(None) => Ok("default".to_string()),
            Err(error) => Err(map_session_error(error)),
        }
    }

    fn attach_session(&mut self, name: &str) -> Result<AttachInfo, ProtocolError> {
        validate_session_name(name)?;
        let key = key_of(name);
        if self.sessions.contains_key(&key) {
            self.attach_live(&key)
        } else if self.store.exists(name).map_err(map_session_error)? {
            Err(ProtocolError::new(
                ErrorCode::Conflict,
                format!("session '{name}' is not running; use `fact resume --session {name}`"),
            ))
        } else {
            Err(ProtocolError::not_found(format!(
                "session '{name}' does not exist"
            )))
        }
    }

    fn attach_live(&mut self, key: &str) -> Result<AttachInfo, ProtocolError> {
        let token = fact_runtime::generate_token()
            .map_err(|error| ProtocolError::internal(error.to_string()))?;
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session.last_active_ms = now_ms();
        let summary = session.data.summary(true);
        let cold_recovered = session.cold_recovered;
        let recovery_notes = session.recovery_notes.clone();
        let session_key = key.to_string();
        self.stream_grants.insert(
            token.clone(),
            StreamGrant {
                session_key,
                created: Instant::now(),
            },
        );
        Ok(AttachInfo {
            session: summary,
            stream_token: token,
            cold_recovered,
            recovery_notes,
        })
    }

    fn create_session(
        &mut self,
        name: String,
        grid: u8,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<AttachInfo, ProtocolError> {
        validate_session_name(&name)?;
        if self.sessions.len() >= self.config.max_sessions.min(MAX_SESSIONS) {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                format!(
                    "maximum live session count reached ({})",
                    self.config.max_sessions
                ),
            ));
        }
        if self.store.exists(&name).map_err(map_session_error)? {
            return Err(ProtocolError::new(
                ErrorCode::Conflict,
                format!("session '{name}' already exists"),
            ));
        }
        let resolved = self
            .config
            .resolve_profile(profile)
            .map_err(map_config_error)?;
        let count = grid.clamp(1, MAX_GRID_PANES) as usize;
        let ids: Vec<u64> = (0..count).map(|_| self.alloc_pane_id()).collect();
        let layout = SessionLayout::new(1, &ids);
        let options = LayoutOptions {
            cols: DEFAULT_VIEWPORT_COLS,
            rows: DEFAULT_VIEWPORT_ROWS - 2,
            min_content_cols: self.config.min_pane_width,
            min_content_rows: self.config.min_pane_height,
        };
        let layout_result = layout.layout(options);
        let dims: HashMap<u64, (u16, u16)> = layout_result
            .panes
            .iter()
            .map(|slot| {
                (
                    slot.pane,
                    (slot.content.width.max(1), slot.content.height.max(1)),
                )
            })
            .collect();
        let mut panes = HashMap::new();
        let mut records = Vec::new();
        let mut notes = Vec::new();
        // Only an explicit profile title is preserved verbatim. Automatic
        // labels are normalized from the resolved executable by spawn_pane.
        let profile_title = resolved.title.clone().unwrap_or_default();
        for id in &ids {
            let (cols, rows) = dims.get(id).copied().unwrap_or((80, 24));
            let spawned = spawn_pane(
                &self.config,
                PaneSpec {
                    profile,
                    cwd,
                    cols,
                    rows,
                    snapshot: None,
                    title_hint: &profile_title,
                    cold_recovery: false,
                },
                &mut notes,
            )?;
            let record = PaneRecord {
                id: *id,
                title: spawned.display_title.clone(),
                pid: spawned.pty.pid(),
                exit_code: None,
                profile: profile.map(str::to_string),
                cwd: cwd.map(str::to_string),
                snapshot: None,
            };
            panes.insert(*id, spawned);
            records.push(record);
        }
        let mut data = SessionData::new(name.clone(), layout, records);
        data.updated_at_ms = now_ms();
        if let Err(error) = self.store.save(&data) {
            return Err(map_session_error(error));
        }
        let session = LiveSession {
            data,
            panes,
            viewport_cols: DEFAULT_VIEWPORT_COLS,
            viewport_rows: DEFAULT_VIEWPORT_ROWS,
            layout_epoch: 1,
            metadata_epoch: 1,
            detach_seq: 0,
            cold_recovered: false,
            recovery_notes: notes,
            last_persist: Instant::now(),
            last_snapshot: Instant::now(),
            last_active_ms: now_ms(),
        };
        let key = key_of(&name);
        self.sessions.insert(key.clone(), session);
        self.attach_live(&key)
    }

    fn cold_recover(&mut self, mut data: SessionData) -> Result<(), ProtocolError> {
        let old_ids: Vec<u64> = data.layout.all_panes_including_minimized();
        let mut remap = BTreeMap::new();
        for old in old_ids {
            remap.insert(old, self.alloc_pane_id());
        }
        data.layout.remap_panes(&remap).map_err(map_layout_error)?;
        for record in &mut data.panes {
            let new = remap.get(&record.id).copied().ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::Internal,
                    "saved layout and pane records are inconsistent",
                )
            })?;
            record.id = new;
        }

        let options = LayoutOptions {
            cols: DEFAULT_VIEWPORT_COLS,
            rows: DEFAULT_VIEWPORT_ROWS - 2,
            min_content_cols: self.config.min_pane_width,
            min_content_rows: self.config.min_pane_height,
        };
        let layout_result = data.layout.layout(options);
        let dims: HashMap<u64, (u16, u16)> = layout_result
            .panes
            .iter()
            .map(|slot| {
                (
                    slot.pane,
                    (slot.content.width.max(1), slot.content.height.max(1)),
                )
            })
            .collect();

        let mut panes = HashMap::new();
        let mut notes = vec![
            "The previous shell processes were lost; new shells were started.".to_string(),
            "Commands were not replayed automatically.".to_string(),
        ];
        let mut had_snapshot = false;
        for record in &data.panes {
            let (cols, rows) = dims.get(&record.id).copied().unwrap_or((80, 24));
            had_snapshot |= record.snapshot.is_some();
            let spawned = spawn_pane(
                &self.config,
                PaneSpec {
                    profile: record.profile.as_deref(),
                    cwd: record.cwd.as_deref(),
                    cols,
                    rows,
                    snapshot: record.snapshot.as_ref(),
                    title_hint: &record.title,
                    cold_recovery: true,
                },
                &mut notes,
            )?;
            panes.insert(record.id, spawned);
        }
        if had_snapshot {
            notes.insert(
                0,
                "Saved output was replayed from the last snapshot.".to_string(),
            );
        } else {
            notes.insert(
                0,
                "No saved output was available for these panes.".to_string(),
            );
        }
        for record in &mut data.panes {
            record.pid = panes.get(&record.id).map_or(0, |pane| pane.pty.pid());
            record.exit_code = None;
        }
        data.cold_recovered_at_ms = Some(now_ms());
        data.updated_at_ms = now_ms();
        let key = key_of(&data.name);
        let session = LiveSession {
            data,
            panes,
            viewport_cols: DEFAULT_VIEWPORT_COLS,
            viewport_rows: DEFAULT_VIEWPORT_ROWS,
            layout_epoch: 1,
            metadata_epoch: 1,
            detach_seq: 0,
            cold_recovered: true,
            recovery_notes: notes,
            last_persist: Instant::now(),
            last_snapshot: Instant::now(),
            last_active_ms: now_ms(),
        };
        self.sessions.insert(key.clone(), session);
        if let Err(error) = self.persist_session(&key) {
            if let Some(session) = self.sessions.remove(&key) {
                for (_, pane) in session.panes {
                    pane.pty.kill();
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionSummary>, ProtocolError> {
        let metas = self
            .store
            .list(include_archived)
            .map_err(map_session_error)?;
        let mut list: Vec<SessionSummary> = metas
            .iter()
            .map(|meta: &SessionMeta| meta.to_summary(false))
            .collect();
        for (key, session) in &self.sessions {
            let summary = session.data.summary(true);
            if let Some(existing) = list.iter_mut().find(|item| key_of(&item.name) == *key) {
                *existing = summary;
            } else if include_archived || !summary.archived {
                list.push(summary);
            }
        }
        list.sort_by(|a, b| {
            a.archived
                .cmp(&b.archived)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(list)
    }

    fn rename_session(
        &mut self,
        name: &str,
        new_name: &str,
    ) -> Result<SessionSummary, ProtocolError> {
        validate_session_name(new_name)?;
        let key = key_of(name);
        let new_key = key_of(new_name);
        if key != new_key && self.sessions.contains_key(&new_key) {
            return Err(ProtocolError::new(
                ErrorCode::Conflict,
                format!("session '{new_name}' is live"),
            ));
        }
        if key != new_key && self.store.exists(new_name).map_err(map_session_error)? {
            return Err(ProtocolError::new(
                ErrorCode::Conflict,
                format!("session '{new_name}' already exists"),
            ));
        }
        self.store
            .rename(name, new_name)
            .map_err(map_session_error)?;
        if let Some(mut session) = self.sessions.remove(&key) {
            session.data.name = new_name.to_string();
            session.data.updated_at_ms = now_ms();
            for subscriber in &mut self.subscribers {
                if subscriber.session_key == key {
                    subscriber.session_key = new_key.clone();
                    subscriber.metadata_epoch = 0;
                }
            }
            for grant in self.stream_grants.values_mut() {
                if grant.session_key == key {
                    grant.session_key = new_key.clone();
                }
            }
            let summary = session.data.summary(true);
            self.sessions.insert(new_key, session);
            Ok(summary)
        } else {
            let saved = self.store.load(new_name).map_err(map_session_error)?;
            Ok(saved.summary(false))
        }
    }

    fn archive_session(
        &mut self,
        name: &str,
        archived: bool,
    ) -> Result<SessionSummary, ProtocolError> {
        let key = self.session_key_opt(name);
        if archived {
            if let Some(key) = &key {
                self.terminate_live(key, true)?;
            }
        }
        self.store
            .set_archived(name, archived)
            .map_err(map_session_error)?;
        let saved = self.store.load(name).map_err(map_session_error)?;
        Ok(saved.summary(false))
    }

    fn delete_session(&mut self, name: &str, force: bool) -> Result<(), ProtocolError> {
        if let Some(key) = self.session_key_opt(name) {
            if !force {
                return Err(ProtocolError::new(
                    ErrorCode::Conflict,
                    "session is live; terminate it first or use force",
                ));
            }
            self.terminate_live(&key, false)?;
        }
        self.store.delete(name).map_err(map_session_error)
    }

    fn terminate_session(&mut self, name: &str) -> Result<(), ProtocolError> {
        let key = self.session_key(name)?;
        self.terminate_live(&key, true).map(|_| ())
    }

    fn terminate_live(&mut self, key: &str, save: bool) -> Result<SessionData, ProtocolError> {
        if save {
            self.snapshot_session(key)?;
            self.persist_session(key)?;
        }
        let Some(session) = self.sessions.remove(key) else {
            return Err(ProtocolError::not_found("session is not running"));
        };
        self.remove_stream_grants(key);
        self.end_subscribers(key, "session terminated");
        for (_, pane) in session.panes {
            pane.pty.kill();
            drop(pane);
        }
        Ok(session.data)
    }

    fn detach_clients(&mut self, name: Option<&str>) -> Result<(), ProtocolError> {
        if let Some(name) = name {
            let key = self.session_key(name)?;
            let session = self
                .sessions
                .get_mut(&key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            session.detach_seq = session.detach_seq.wrapping_add(1);
            session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
            return Ok(());
        }
        for session in self.sessions.values_mut() {
            session.detach_seq = session.detach_seq.wrapping_add(1);
            session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        }
        Ok(())
    }

    fn write_input(
        &mut self,
        pane: u64,
        data: &[u8],
        bound_session: Option<&str>,
    ) -> Result<(), ProtocolError> {
        if data.len() > MAX_INPUT_CHUNK {
            return Err(ProtocolError::invalid("input chunk is too large"));
        }
        let key = self.pane_owner(pane, bound_session)?;
        let live = self
            .sessions
            .get_mut(&key)
            .and_then(|session| session.panes.get_mut(&pane))
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        live.pty
            .write(data)
            .map_err(|error| ProtocolError::invalid(error.to_string()))?;
        live.term.reset_scroll();
        live.version = live.version.wrapping_add(1);
        Ok(())
    }

    fn paste(
        &mut self,
        pane: u64,
        text: &str,
        bound_session: Option<&str>,
    ) -> Result<(), ProtocolError> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(ProtocolError::invalid("paste is too large"));
        }
        let key = self.pane_owner(pane, bound_session)?;
        let live = self
            .sessions
            .get_mut(&key)
            .and_then(|session| session.panes.get_mut(&pane))
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if live.term.bracketed_paste() {
            format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
        } else {
            normalized.into_bytes()
        };
        live.pty
            .write(&bytes)
            .map_err(|error| ProtocolError::invalid(error.to_string()))?;
        live.version = live.version.wrapping_add(1);
        Ok(())
    }

    fn resize_viewport(&mut self, key: &str, cols: u16, rows: u16) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let cols = cols.clamp(10, 1000);
        let rows = rows.clamp(4, 500);
        if session.viewport_cols == cols && session.viewport_rows == rows {
            return Ok(());
        }
        session.viewport_cols = cols;
        session.viewport_rows = rows;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn resize_session(&mut self, key: &str) {
        let min_cols = self.config.min_pane_width;
        let min_rows = self.config.min_pane_height;
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        let options = LayoutOptions {
            cols: session.viewport_cols,
            rows: session.viewport_rows.saturating_sub(2).max(1),
            min_content_cols: min_cols,
            min_content_rows: min_rows,
        };
        let layout = session.data.layout.layout(options);
        for slot in &layout.panes {
            let width = slot.content.width.max(1);
            let height = slot.content.height.max(1);
            if let Some(pane) = session.panes.get_mut(&slot.pane) {
                if pane.term.cols() != width || pane.term.rows() != height {
                    pane.term.resize(width, height);
                    let _ = pane.pty.resize(width, height);
                    pane.version = pane.version.wrapping_add(1);
                }
            }
        }
    }

    fn split_pane(
        &mut self,
        key: &str,
        target: u64,
        direction: fact_protocol::SplitDirection,
    ) -> Result<(), ProtocolError> {
        let options = self
            .sessions
            .get(key)
            .map(|session| self.layout_options(session))
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let (profile, cwd, cols, rows) = {
            let session = self
                .sessions
                .get(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            let record = session
                .data
                .pane(target)
                .ok_or_else(|| ProtocolError::not_found(format!("pane {target}")))?;
            let layout = session.data.layout.layout(options);
            let slot = layout
                .panes
                .iter()
                .find(|slot| slot.pane == target)
                .ok_or_else(|| ProtocolError::not_found("pane is not visible"))?;
            let (cols, rows) = match direction {
                fact_protocol::SplitDirection::Right => {
                    (slot.content.width / 2, slot.content.height)
                }
                fact_protocol::SplitDirection::Down => {
                    (slot.content.width, slot.content.height / 2)
                }
            };
            (
                record.profile.clone(),
                record.cwd.clone(),
                cols.max(1),
                rows.max(1),
            )
        };
        let id = self.alloc_pane_id();
        let mut notes = Vec::new();
        let spawned = spawn_pane(
            &self.config,
            PaneSpec {
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                snapshot: None,
                title_hint: "",
                cold_recovery: false,
            },
            &mut notes,
        )?;
        let pid = spawned.pty.pid();
        let title = spawned.display_title.clone();
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        if session.panes.len() >= MAX_PANES_PER_SESSION {
            return Err(ProtocolError::new(ErrorCode::Busy, "pane limit reached"));
        }
        let layout_direction = match direction {
            fact_protocol::SplitDirection::Right => LayoutSplitDirection::Right,
            fact_protocol::SplitDirection::Down => LayoutSplitDirection::Down,
        };
        let tab = session
            .data
            .layout
            .split_pane(target, id, layout_direction)
            .map_err(map_layout_error)?;
        session.data.layout.active_tab = tab;
        session.data.panes.push(PaneRecord {
            id,
            title: title.clone(),
            pid,
            exit_code: None,
            profile,
            cwd,
            snapshot: None,
        });
        session.panes.insert(id, spawned);
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn close_pane(&mut self, key: &str, pane: u64) -> Result<(), ProtocolError> {
        let remove_session;
        {
            let session = self
                .sessions
                .get_mut(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            if !session.panes.contains_key(&pane) {
                return Err(ProtocolError::not_found(format!("pane {pane}")));
            }
            let tab_id = session
                .data
                .layout
                .tab_for_pane(pane)
                .or_else(|| {
                    session
                        .data
                        .layout
                        .tabs
                        .iter()
                        .find(|tab| tab.minimized.contains(&pane))
                        .map(|tab| tab.id)
                })
                .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
            let was_minimized = session
                .data
                .layout
                .tab(tab_id)
                .is_some_and(|tab| tab.minimized.contains(&pane));
            if was_minimized {
                if let Some(tab) = session.data.layout.tab_mut(tab_id) {
                    tab.minimized.retain(|item| *item != pane);
                }
            } else {
                session
                    .data
                    .layout
                    .remove_pane(pane)
                    .map_err(map_layout_error)?;
            }
            session.data.panes.retain(|record| record.id != pane);
            if let Some(live) = session.panes.remove(&pane) {
                live.pty.kill();
                drop(live);
            }
            let tab_empty = session
                .data
                .layout
                .tab(tab_id)
                .is_some_and(|tab| tab.root.is_none() && tab.minimized.is_empty());
            if tab_empty {
                session
                    .data
                    .layout
                    .remove_tab(tab_id)
                    .map_err(map_layout_error)?;
            }
            session.layout_epoch = session.layout_epoch.wrapping_add(1);
            session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
            remove_session = session.data.layout.tabs.is_empty();
        }
        if remove_session {
            self.end_live_empty(key);
        } else {
            self.resize_session(key);
        }
        Ok(())
    }

    fn restart_pane(&mut self, key: &str, pane: u64) -> Result<(), ProtocolError> {
        let (profile, cwd, cols, rows, title) = {
            let session = self
                .sessions
                .get(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            let live = session
                .panes
                .get(&pane)
                .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
            let record = session.data.pane(pane);
            (
                record.and_then(|r| r.profile.clone()),
                record.and_then(|r| r.cwd.clone()),
                live.term.cols(),
                live.term.rows(),
                live.display_title.clone(),
            )
        };
        let mut notes = Vec::new();
        let mut spawned = spawn_pane(
            &self.config,
            PaneSpec {
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                snapshot: None,
                title_hint: &title,
                cold_recovery: false,
            },
            &mut notes,
        )?;
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let Some(old) = session.panes.remove(&pane) else {
            return Err(ProtocolError::not_found(format!("pane {pane}")));
        };
        let version = old.version.wrapping_add(1);
        old.pty.kill();
        drop(old);
        spawned.version = version;
        spawned.default_title = title;
        spawned.display_title = spawned.default_title.clone();
        let pid = spawned.pty.pid();
        session.panes.insert(pane, spawned);
        if let Some(record) = session.data.pane_mut(pane) {
            record.pid = pid;
            record.exit_code = None;
        }
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        Ok(())
    }

    fn focus_pane(&mut self, key: &str, pane: u64) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .focus_pane(pane)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn focus_direction(
        &mut self,
        key: &str,
        direction: FocusDirection,
    ) -> Result<(), ProtocolError> {
        let target = {
            let session = self
                .sessions
                .get(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            let options = self.layout_options(session);
            let layout = session.data.layout.layout(options);
            let current = layout
                .focused
                .and_then(|pane| layout.panes.iter().find(|slot| slot.pane == pane))
                .ok_or_else(|| ProtocolError::invalid("no focused pane"))?;
            let cx = current.slot.x as i32 * 2 + current.slot.width as i32;
            let cy = current.slot.y as i32 * 2 + current.slot.height as i32;
            layout
                .panes
                .iter()
                .filter(|slot| slot.pane != current.pane)
                .filter_map(|slot| {
                    let sx = slot.slot.x as i32 * 2 + slot.slot.width as i32;
                    let sy = slot.slot.y as i32 * 2 + slot.slot.height as i32;
                    let (primary, secondary) = match direction {
                        FocusDirection::Left if sx < cx => (cx - sx, (sy - cy).abs()),
                        FocusDirection::Right if sx > cx => (sx - cx, (sy - cy).abs()),
                        FocusDirection::Up if sy < cy => (cy - sy, (sx - cx).abs()),
                        FocusDirection::Down if sy > cy => (sy - cy, (sx - cx).abs()),
                        _ => return None,
                    };
                    Some((primary, secondary, slot.pane))
                })
                .min_by_key(|(primary, secondary, _)| (*primary, *secondary))
                .map(|(_, _, pane)| pane)
                .ok_or_else(|| ProtocolError::invalid("no pane in that direction"))?
        };
        self.focus_pane(key, target)
    }

    fn cycle_focus(&mut self, key: &str, delta: isize) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        if session.data.layout.cycle_focus(delta).is_none() {
            return Err(ProtocolError::not_found("no panes to focus"));
        }
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn zoom(&mut self, key: &str, zoomed: bool) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session.data.layout.zoom(zoomed).map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn minimize_pane(&mut self, key: &str, pane: u64) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .hide_pane(pane)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn restore_panes(&mut self, key: &str) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let restored = session.data.layout.restore_all_minimized();
        if restored.is_empty() {
            return Err(ProtocolError::invalid("no minimized panes"));
        }
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn new_tab(
        &mut self,
        key: &str,
        profile: Option<String>,
        cwd: Option<String>,
    ) -> Result<(), ProtocolError> {
        let (cols, rows) = {
            let session = self
                .sessions
                .get(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            let options = self.layout_options(session);
            let layout = session.data.layout.layout(options);
            layout
                .panes
                .iter()
                .find(|slot| Some(slot.pane) == layout.focused)
                .map(|slot| (slot.content.width.max(1), slot.content.height.max(1)))
                .unwrap_or((80, 24))
        };
        let mut notes = Vec::new();
        let spawned = spawn_pane(
            &self.config,
            PaneSpec {
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                snapshot: None,
                title_hint: "",
                cold_recovery: false,
            },
            &mut notes,
        )?;
        let id = self.alloc_pane_id();
        let title = spawned.display_title.clone();
        let pid = spawned.pty.pid();
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        if session.panes.len() >= MAX_PANES_PER_SESSION {
            return Err(ProtocolError::new(ErrorCode::Busy, "pane limit reached"));
        }
        let tab_id = session
            .data
            .layout
            .tabs
            .iter()
            .map(|tab| tab.id)
            .max()
            .unwrap_or(0)
            .wrapping_add(1);
        session.data.layout.add_tab(tab_id, &[id], title);
        session.data.panes.push(PaneRecord {
            id,
            title: spawned.display_title.clone(),
            pid,
            exit_code: None,
            profile,
            cwd,
            snapshot: None,
        });
        session.panes.insert(id, spawned);
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn close_tab(&mut self, key: &str, tab: u32) -> Result<(), ProtocolError> {
        let remove_session;
        {
            let session = self
                .sessions
                .get_mut(key)
                .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
            let panes = session
                .data
                .layout
                .remove_tab(tab)
                .map_err(map_layout_error)?;
            for pane in &panes {
                if let Some(live) = session.panes.remove(pane) {
                    live.pty.kill();
                    drop(live);
                }
                session.data.panes.retain(|record| record.id != *pane);
            }
            session.layout_epoch = session.layout_epoch.wrapping_add(1);
            session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
            remove_session = session.data.layout.tabs.is_empty();
        }
        if remove_session {
            self.end_live_empty(key);
        } else {
            self.resize_session(key);
        }
        Ok(())
    }

    fn select_tab(&mut self, key: &str, tab: u32) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .select_tab(tab)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn rename_tab(&mut self, key: &str, tab: u32, title: &str) -> Result<(), ProtocolError> {
        if title.chars().count() > 80 {
            return Err(ProtocolError::invalid("tab title is too long"));
        }
        let sanitized = sanitize_title(title);
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let tab = session
            .data
            .layout
            .tab_mut(tab)
            .ok_or_else(|| ProtocolError::not_found(format!("tab {tab}")))?;
        tab.title = sanitized;
        tab.custom_title = true;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        Ok(())
    }

    fn change_tab(&mut self, key: &str, delta: isize) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        if session.data.layout.tabs.len() < 2 {
            return Ok(());
        }
        session.data.layout.next_tab(delta);
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn move_pane(&mut self, key: &str, pane: u64, target_tab: u32) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .move_pane_to_tab(pane, target_tab)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn swap_panes(&mut self, key: &str, a: u64, b: u64) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .swap_panes(a, b)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn scroll(&mut self, key: &str, pane: u64, delta: i32) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let live = session
            .panes
            .get_mut(&pane)
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        live.term.scroll(delta);
        live.version = live.version.wrapping_add(1);
        Ok(())
    }

    fn scroll_to_line(&mut self, key: &str, pane: u64, line: i32) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let live = session
            .panes
            .get_mut(&pane)
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        live.term.scroll_to_line(line);
        live.version = live.version.wrapping_add(1);
        Ok(())
    }

    fn set_scroll_offset(
        &mut self,
        key: &str,
        pane: u64,
        offset: usize,
    ) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let live = session
            .panes
            .get_mut(&pane)
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        live.term.set_scroll_offset(offset);
        live.version = live.version.wrapping_add(1);
        Ok(())
    }

    fn search(
        &self,
        key: &str,
        pane: u64,
        query: &str,
        case_sensitive: bool,
        limit: u16,
    ) -> Result<SearchResult, ProtocolError> {
        if query.len() > fact_protocol::MAX_SEARCH_QUERY {
            return Err(ProtocolError::invalid("search query is too long"));
        }
        if query.is_empty() {
            return Err(ProtocolError::invalid("search query is empty"));
        }
        let session = self
            .sessions
            .get(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        let live = session
            .panes
            .get(&pane)
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))?;
        let (hits, truncated) =
            live.term
                .search(query, case_sensitive, usize::from(limit.clamp(1, 1000)));
        Ok(SearchResult {
            pane,
            query: query.to_string(),
            hits: hits
                .into_iter()
                .map(|hit| SearchHit {
                    line: hit.line,
                    column: hit.column,
                    preview: hit.preview,
                })
                .collect(),
            truncated,
        })
    }

    fn drag_split(&mut self, key: &str, path: &[u8], ratio: f32) -> Result<(), ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        session
            .data
            .layout
            .set_split_ratio(path, ratio)
            .map_err(map_layout_error)?;
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(())
    }

    fn snapshot_session(&mut self, key: &str) -> Result<(), ProtocolError> {
        let max_bytes = self.config.snapshot_max_bytes;
        let now = now_ms();
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        for (id, pane) in &mut session.panes {
            let snapshot = pane.term.recovery_snapshot(max_bytes, now);
            if let Some(record) = session.data.pane_mut(*id) {
                record.snapshot = Some(PaneSnapshot::new(
                    snapshot.ansi,
                    snapshot.cols,
                    snapshot.rows,
                    snapshot.format_version,
                    snapshot.captured_at_ms,
                    snapshot.truncated,
                ));
            }
        }
        session.last_snapshot = Instant::now();
        Ok(())
    }

    fn persist_session(&mut self, key: &str) -> Result<(), ProtocolError> {
        let now = now_ms();
        let SessionManager {
            sessions, store, ..
        } = self;
        let session = sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        for (id, pane) in &session.panes {
            if let Some(record) = session.data.pane_mut(*id) {
                record.title = pane.display_title.clone();
                record.pid = pane.pty.pid();
                record.exit_code = pane
                    .exit_code
                    .or_else(|| pane.pty.exit().map(|exit| exit.code));
                record.profile = pane.profile.clone();
                record.cwd = pane.cwd.clone();
            }
        }
        session.data.updated_at_ms = now;
        store.save(&session.data).map_err(map_session_error)?;
        session.last_persist = Instant::now();
        Ok(())
    }

    fn end_live_empty(&mut self, key: &str) {
        if let Some(session) = self.sessions.remove(key) {
            let name = session.data.name.clone();
            for (_, pane) in session.panes {
                pane.pty.kill();
                drop(pane);
            }
            self.remove_stream_grants(key);
            self.end_subscribers(key, "session closed");
            let _ = self.store.delete(&name);
        }
    }

    fn end_subscribers(&mut self, key: &str, reason: &str) {
        let mut retained = Vec::with_capacity(self.subscribers.len());
        for subscriber in self.subscribers.drain(..) {
            if subscriber.session_key == key {
                let _ = subscriber
                    .tx
                    .try_send(fact_protocol::ServerMessage::SessionEnded {
                        reason: reason.to_string(),
                    });
            } else {
                retained.push(subscriber);
            }
        }
        self.subscribers = retained;
    }

    fn remove_stream_grants(&mut self, key: &str) {
        self.stream_grants
            .retain(|_, grant| grant.session_key != key);
    }

    fn pump_frames(&mut self) {
        let mut index = 0;
        while index < self.subscribers.len() {
            let Some(built) = self.build_frame_for(index) else {
                index += 1;
                continue;
            };
            let BuiltFrame {
                mut frame,
                versions,
                layout_epoch,
                metadata_epoch,
            } = built;
            frame.seq = self.next_frame_seq;
            match self.subscribers[index]
                .tx
                .try_send(fact_protocol::ServerMessage::Frame(Box::new(frame)))
            {
                Ok(()) => {
                    self.next_frame_seq = self.next_frame_seq.wrapping_add(1);
                    let subscriber = &mut self.subscribers[index];
                    for (pane, version) in versions {
                        subscriber.sent_versions.insert(pane, version);
                    }
                    subscriber.layout_epoch = layout_epoch;
                    subscriber.metadata_epoch = metadata_epoch;
                    index += 1;
                }
                Err(TrySendError::Full(_)) => index += 1,
                Err(TrySendError::Disconnected(_)) => {
                    self.subscribers.remove(index);
                }
            }
        }
    }

    fn build_frame_for(&mut self, index: usize) -> Option<BuiltFrame> {
        let key = self.subscribers.get(index)?.session_key.clone();
        let min_cols = self.config.min_pane_width;
        let min_rows = self.config.min_pane_height;
        let subscriber = self.subscribers.get_mut(index)?;
        let session = self.sessions.get_mut(&key)?;
        let layout = session.data.layout.layout(LayoutOptions {
            cols: session.viewport_cols,
            rows: session.viewport_rows.saturating_sub(2).max(1),
            min_content_cols: min_cols,
            min_content_rows: min_rows,
        });
        let layout_changed = subscriber.layout_epoch != session.layout_epoch;
        if layout_changed {
            subscriber.sent_versions.clear();
        }
        let mut needed = layout_changed || subscriber.metadata_epoch != session.metadata_epoch;
        for slot in &layout.panes {
            if let Some(pane) = session.panes.get(&slot.pane) {
                if subscriber.sent_versions.get(&slot.pane) != Some(&pane.version) {
                    needed = true;
                }
            }
        }
        if !needed {
            return None;
        }

        let active_tab = session.data.layout.active_tab;
        let mut versions = Vec::with_capacity(layout.panes.len());
        let mut pane_views = Vec::with_capacity(layout.panes.len());
        for slot in &layout.panes {
            let Some(pane) = session.panes.get_mut(&slot.pane) else {
                continue;
            };
            let snapshot = pane.term.snapshot();
            let send_cells =
                layout_changed || subscriber.sent_versions.get(&slot.pane) != Some(&pane.version);
            versions.push((slot.pane, pane.version));
            pane_views.push(PaneView {
                id: slot.pane,
                tab: active_tab,
                title: pane.display_title.clone(),
                pid: pane.pty.pid(),
                alive: pane.exit_code.is_none() && pane.pty.is_alive(),
                exit_code: pane
                    .exit_code
                    .or_else(|| pane.pty.exit().map(|exit| exit.code)),
                slot: fact_protocol::RectDto {
                    x: slot.slot.x,
                    y: slot.slot.y,
                    width: slot.slot.width,
                    height: slot.slot.height,
                },
                content_cols: pane.term.cols(),
                content_rows: pane.term.rows(),
                version: pane.version,
                cells: if send_cells {
                    Some(snapshot.cells)
                } else {
                    None
                },
                cursor: CursorDto {
                    x: snapshot.cursor.x,
                    y: snapshot.cursor.y,
                    visible: snapshot.cursor.visible,
                    shape: snapshot.cursor.shape,
                },
                modes: snapshot.modes,
                scroll_offset: snapshot.scroll_offset,
                history_lines: snapshot.history_lines,
                bell_seq: snapshot.bell_seq,
                cold_restored: pane.term.cold_restored(),
                focused: Some(slot.pane) == layout.focused,
            });
        }

        let tabs = session
            .data
            .layout
            .tabs
            .iter()
            .map(|tab| TabView {
                id: tab.id,
                title: tab.title.clone(),
                active: tab.id == active_tab,
                pane_count: tab_pane_count(tab.root.as_ref(), tab.minimized.len()),
                zoomed: tab.zoomed,
            })
            .collect();
        let minimized = session
            .data
            .layout
            .active()
            .map(|tab| {
                tab.minimized
                    .iter()
                    .filter_map(|pane_id| {
                        session.panes.get(pane_id).map(|pane| PaneSummary {
                            id: *pane_id,
                            title: pane.display_title.clone(),
                            pid: pane.pty.pid(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let splits = layout
            .splits
            .iter()
            .map(|split| SplitHandle {
                path: split.path.clone(),
                orientation: match split.orientation {
                    fact_layout::SplitOrientation::Horizontal => SplitOrientation::Horizontal,
                    fact_layout::SplitOrientation::Vertical => SplitOrientation::Vertical,
                },
                region: fact_protocol::RectDto {
                    x: split.region.x,
                    y: split.region.y,
                    width: split.region.width,
                    height: split.region.height,
                },
                ratio: split.ratio,
            })
            .collect();

        let frame = Frame {
            protocol: fact_protocol::PROTOCOL_VERSION,
            seq: 0,
            session: session.data.summary(true),
            tabs,
            active_tab,
            focus: layout.focused,
            panes: pane_views,
            minimized,
            splits,
            viewport: Viewport {
                cols: session.viewport_cols,
                rows: session.viewport_rows,
                slot_rows: session.viewport_rows.saturating_sub(2).max(1),
                slot_cols: session.viewport_cols,
            },
            tiny_mode: layout.tiny_mode,
            detach_seq: session.detach_seq,
            server_time_ms: now_ms(),
        };
        Some(BuiltFrame {
            frame,
            versions,
            layout_epoch: session.layout_epoch,
            metadata_epoch: session.metadata_epoch,
        })
    }

    fn layout_options(&self, session: &LiveSession) -> LayoutOptions {
        LayoutOptions {
            cols: session.viewport_cols,
            rows: session.viewport_rows.saturating_sub(2).max(1),
            min_content_cols: self.config.min_pane_width,
            min_content_rows: self.config.min_pane_height,
        }
    }

    fn alloc_pane_id(&mut self) -> u64 {
        let id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1).max(1);
        id
    }

    fn session_key(&self, name: &str) -> Result<String, ProtocolError> {
        validate_session_name(name)?;
        let key = key_of(name);
        if self.sessions.contains_key(&key) {
            Ok(key)
        } else if self.store.exists(name).map_err(map_session_error)? {
            Err(ProtocolError::new(
                ErrorCode::Conflict,
                format!("session '{name}' is not running"),
            ))
        } else {
            Err(ProtocolError::not_found(format!("session '{name}'")))
        }
    }

    fn session_key_opt(&self, name: &str) -> Option<String> {
        let key = key_of(name);
        self.sessions.contains_key(&key).then_some(key)
    }

    fn require_bound(&self, bound: Option<&str>) -> Result<String, ProtocolError> {
        let name = bound.ok_or_else(|| {
            ProtocolError::invalid("attach to a session before sending layout commands")
        })?;
        self.session_key(name)
    }

    fn pane_owner(&self, pane: u64, bound: Option<&str>) -> Result<String, ProtocolError> {
        if let Some(bound) = bound {
            let key = key_of(bound);
            if self
                .sessions
                .get(&key)
                .is_some_and(|session| session.panes.contains_key(&pane))
            {
                return Ok(key);
            }
            return Err(ProtocolError::not_found(format!(
                "pane {pane} is not in session '{bound}'"
            )));
        }
        self.sessions
            .iter()
            .find(|(_, session)| session.panes.contains_key(&pane))
            .map(|(key, _)| key.clone())
            .ok_or_else(|| ProtocolError::not_found(format!("pane {pane}")))
    }
}

fn spawn_pane(
    config: &Config,
    spec: PaneSpec<'_>,
    notes: &mut Vec<String>,
) -> Result<LivePane, ProtocolError> {
    let PaneSpec {
        profile,
        cwd,
        cols,
        rows,
        snapshot,
        title_hint,
        cold_recovery,
    } = spec;
    let resolved = config.resolve_profile(profile).map_err(map_config_error)?;
    let mut workdir = cwd
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            fact_config::expand_path(value).map_err(|error| {
                ProtocolError::invalid(format!("working directory is invalid: {error}"))
            })
        })
        .transpose()?;
    if let Some(dir) = &workdir {
        if !dir.is_dir() {
            notes.push(format!(
                "Working directory '{}' is unavailable; using the profile default.",
                dir.display()
            ));
            workdir = None;
        }
    }
    let workdir = workdir.or_else(|| {
        resolved.cwd.clone().filter(|dir| {
            if dir.is_dir() {
                true
            } else {
                notes.push(format!(
                    "Profile working directory '{}' is unavailable; using the daemon directory.",
                    dir.display()
                ));
                false
            }
        })
    });

    let title = if title_hint.trim().is_empty() {
        if let Some(explicit) = &resolved.title {
            // An explicit profile title is user-authored: keep slashes and
            // other punctuation that friendly_title would treat as a path.
            sanitize_title(explicit)
        } else {
            fact_protocol::friendly_title(&resolved.label())
        }
    } else {
        sanitize_title(title_hint)
    };
    let sink = TerminalEventSink::new();
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(16);
    sink.attach_writer(response_tx);
    let initial = snapshot
        .map(|snap| (snap.cols.max(1), snap.rows.max(1)))
        .unwrap_or((cols, rows));
    let mut term = TerminalModel::new(initial.0, initial.1, config.scrollback_lines, sink);
    if cold_recovery {
        if let Some(snapshot) = snapshot {
            if let Err(error) = term.restore_snapshot(&snapshot.ansi) {
                notes.push(format!("A saved snapshot could not be replayed: {error}"));
            }
        } else {
            term.mark_cold_restored();
        }
        term.feed(
            "\r\n\x1b[33m[FACT] Cold recovery: the previous process is gone; a new shell was started.\x1b[0m\r\n\
             \x1b[90m[FACT] Saved output is visual only and commands were not replayed.\x1b[0m\r\n"
                .as_bytes(),
        );
        term.resize(cols, rows);
    }

    let mut options = SpawnOptions::new(resolved.command.clone(), cols, rows);
    options.args = resolved.args.clone();
    options.cwd = workdir.clone();
    options.env = resolved.env.clone();
    let pty = PtyProcess::spawn(options)
        .map_err(|error| ProtocolError::internal(format!("could not start pane: {error}")))?;

    Ok(LivePane {
        pty,
        term,
        responses: response_rx,
        version: 1,
        default_title: title.clone(),
        display_title: title,
        last_bell: 0,
        exit_code: None,
        profile: profile.map(str::to_string),
        cwd: cwd.map(str::to_string),
    })
}

fn first_pane(node: &LayoutNode) -> Option<u64> {
    match node {
        LayoutNode::Leaf { pane } => Some(*pane),
        LayoutNode::Split { first, .. } => first_pane(first),
    }
}

fn tab_pane_count(node: Option<&LayoutNode>, minimized: usize) -> usize {
    fn count(node: &LayoutNode) -> usize {
        match node {
            LayoutNode::Leaf { .. } => 1,
            LayoutNode::Split { first, second, .. } => count(first) + count(second),
        }
    }
    node.map(count).unwrap_or(0) + minimized
}

fn key_of(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn map_session_error(error: SessionError) -> ProtocolError {
    match error {
        SessionError::NotFound(message) => ProtocolError::not_found(message),
        SessionError::AlreadyExists(message) => ProtocolError::new(ErrorCode::Conflict, message),
        SessionError::Invalid(message) | SessionError::Corrupt(message) => {
            ProtocolError::invalid(message)
        }
        SessionError::TooLarge { size } => {
            ProtocolError::invalid(format!("session data is too large ({size} bytes)"))
        }
        SessionError::Schema(version) => ProtocolError::new(
            ErrorCode::Unsupported,
            format!("unsupported database schema version {version}"),
        ),
        other => ProtocolError::internal(other.to_string()),
    }
}

fn map_layout_error(error: fact_layout::LayoutError) -> ProtocolError {
    ProtocolError::invalid(error.to_string())
}

fn map_config_error(error: fact_config::ConfigError) -> ProtocolError {
    match error {
        fact_config::ConfigError::UnknownProfile(_) => {
            ProtocolError::new(ErrorCode::NotFound, error.to_string())
        }
        _ => ProtocolError::invalid(error.to_string()),
    }
}
