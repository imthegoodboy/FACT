//! In-process session state: real panes, layouts and frame generation.
//!
//! All mutation happens on one owner thread. The UI submits commands through a
//! channel, which keeps PTY output, layout changes and frame building serialized
//! without a global lock. No state is written to disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Instant;

use fact_config::Config;
use fact_layout::{
    LayoutNode, LayoutOptions, SessionLayout, SplitDirection as LayoutSplitDirection,
};
use fact_protocol::{
    sanitize_title, Command, CursorDto, EngineStats, ErrorCode, FocusDirection, Frame, PaneSummary,
    PaneView, ProtocolError, ResponsePayload, SearchHit, SearchResult, SplitHandle,
    SplitOrientation, TabView, Viewport, BRACKETED_PASTE_END, BRACKETED_PASTE_START,
    MAX_GRID_PANES, MAX_INPUT_CHUNK, MAX_PASTE_BYTES,
};
use fact_pty::{OutputWaker, PtyProcess, SpawnOptions};
use fact_term::{TerminalEventSink, TerminalModel};

use crate::model::{PaneRecord, SessionData};

const DEFAULT_VIEWPORT_COLS: u16 = 120;
const DEFAULT_VIEWPORT_ROWS: u16 = 35;
const OUTPUT_CHUNKS_PER_TICK: usize = 64;
const MAX_PANES_PER_SESSION: usize = 256;
/// The engine hosts one workspace. Tabs and splits live inside it.
pub(crate) const SESSION_NAME: &str = "FACT";

struct PaneSpec<'a> {
    pane: u64,
    profile: Option<&'a str>,
    cwd: Option<&'a str>,
    cols: u16,
    rows: u16,
    title_hint: &'a str,
}

pub(crate) struct SessionManager {
    config: Config,
    /// Shared by every pane so new output wakes the owner thread at once.
    waker: Option<OutputWaker>,
    sessions: HashMap<String, LiveSession>,
    subscribers: Vec<Subscriber>,
    next_pane_id: u64,
    next_frame_seq: u64,
    shutdown: bool,
    /// The last publish found a subscriber's channel full.
    frames_blocked: bool,
    started: Instant,
}

struct LiveSession {
    data: SessionData,
    panes: HashMap<u64, LivePane>,
    viewport_cols: u16,
    viewport_rows: u16,
    layout_epoch: u64,
    metadata_epoch: u64,
}

/// The tab strip is the only persistent chrome. It appears only when a session
/// has more than one tab, so a single-tab grid uses the full terminal height.
fn chrome_rows(layout: &SessionLayout) -> u16 {
    u16::from(layout.tabs.len() > 1)
}

fn layout_rows(session: &LiveSession) -> u16 {
    session
        .viewport_rows
        .saturating_sub(chrome_rows(&session.data.layout))
        .max(1)
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
    cwd: Option<String>,
}

struct Subscriber {
    tx: SyncSender<fact_protocol::ServerMessage>,
    session_key: String,
    sent_versions: HashMap<u64, u64>,
    layout_epoch: u64,
    metadata_epoch: u64,
}

struct BuiltFrame {
    frame: Frame,
    versions: Vec<(u64, u64)>,
    layout_epoch: u64,
    metadata_epoch: u64,
}

impl SessionManager {
    pub(crate) fn new(
        config: Config,
        grid: u8,
        profile: Option<&str>,
        cwd: Option<&str>,
        waker: Option<OutputWaker>,
    ) -> Result<Self, ProtocolError> {
        let mut manager = Self {
            config,
            waker,
            sessions: HashMap::new(),
            subscribers: Vec::new(),
            next_pane_id: 1,
            next_frame_seq: 1,
            shutdown: false,
            frames_blocked: false,
            started: Instant::now(),
        };
        manager.create_session(SESSION_NAME.to_string(), grid, profile, cwd)?;
        Ok(manager)
    }

    pub(crate) fn add_subscriber(&mut self, tx: SyncSender<fact_protocol::ServerMessage>) {
        self.subscribers.push(Subscriber {
            tx,
            session_key: key_of(SESSION_NAME),
            sent_versions: HashMap::new(),
            layout_epoch: 0,
            metadata_epoch: 0,
        });
        self.pump_frames();
    }

    pub(crate) fn handle(
        &mut self,
        command: Command,
        bound_session: Option<&str>,
    ) -> Result<ResponsePayload, ProtocolError> {
        match command {
            Command::Ping => Ok(ResponsePayload::Pong {
                pid: std::process::id(),
                uptime_ms: self.started.elapsed().as_millis() as u64,
            }),
            Command::Stats => Ok(ResponsePayload::Stats(self.stats())),
            Command::Shutdown => {
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
            Command::ResizePane { direction } => {
                let key = self.require_bound(bound_session)?;
                self.resize_focused(&key, direction)?;
                Ok(ResponsePayload::Ack)
            }
            Command::SetTheme { name } => {
                if !fact_config::is_valid_theme(&name) {
                    return Err(ProtocolError::invalid(format!("unknown theme '{name}'")));
                }
                self.config.theme = name;
                let (foreground, background) = self.config.terminal_colors();
                for session in self.sessions.values() {
                    for pane in session.panes.values() {
                        pane.term.set_default_colors(foreground, background);
                    }
                }
                Ok(ResponsePayload::Ack)
            }
            Command::BalancePanes => {
                let key = self.require_bound(bound_session)?;
                self.edit_layout(&key, |layout| layout.balance().map(|()| true))?;
                Ok(ResponsePayload::Ack)
            }
            Command::MoveTab { tab, index } => {
                let key = self.require_bound(bound_session)?;
                self.edit_layout(&key, |layout| layout.move_tab(tab, index).map(|()| true))?;
                Ok(ResponsePayload::Ack)
            }
        }
    }

    /// Drain output, then publish frames. Returns `true` when a pane still
    /// has output queued, so the owner should come back soon.
    pub(crate) fn tick(&mut self) -> bool {
        let backlog = self.drain_output();
        self.publish();
        backlog
    }

    /// The earliest moment a pane's unfinished synchronized update must be
    /// shown anyway; the owner wakes then even without new output.
    pub(crate) fn next_sync_deadline(&self) -> Option<Instant> {
        self.sessions
            .values()
            .flat_map(|session| session.panes.values())
            .filter_map(|pane| pane.term.sync_deadline())
            .min()
    }

    /// Send every subscriber the frame it needs.
    pub(crate) fn publish(&mut self) {
        self.frames_blocked = self.pump_frames();
    }

    /// A subscriber was too slow to take its last frame. The owner retries
    /// on a gentle timer rather than rebuilding frames at full rate.
    pub(crate) fn frames_blocked(&self) -> bool {
        self.frames_blocked
    }

    /// Feed queued PTY output into the terminal models and answer terminal
    /// queries. This runs as soon as output arrives: agents such as Claude
    /// Code only accept a keyboard-protocol reply that comes back within a
    /// fraction of a millisecond. Returns `true` when a pane still has output
    /// queued beyond this pass's per-pane budget.
    pub(crate) fn drain_output(&mut self) -> bool {
        let mut backlog = false;
        let keys: Vec<String> = self.sessions.keys().cloned().collect();
        for key in keys {
            let mut meta_delta = 0u64;
            if let Some(session) = self.sessions.get_mut(&key) {
                for pane in session.panes.values_mut() {
                    let mut dirty = false;
                    // Observe EOF before draining: checking it afterwards can
                    // race a final chunk arriving just after an empty read.
                    let output_closed = pane.pty.output_closed();
                    let mut output_drained = false;
                    let mut emptied = false;
                    for _ in 0..OUTPUT_CHUNKS_PER_TICK {
                        match pane.pty.try_read() {
                            Some(bytes) => {
                                pane.term.feed(&bytes);
                                dirty = true;
                            }
                            None => {
                                output_drained = output_closed;
                                emptied = true;
                                break;
                            }
                        }
                    }
                    backlog |= !emptied;
                    if pane.term.flush_expired_sync(Instant::now()) {
                        dirty = true;
                    }
                    if dirty {
                        pane.version = pane.version.wrapping_add(1);
                    }
                    while let Ok(response) = pane.responses.try_recv() {
                        if pane.pty.write(&response).is_err() {
                            break;
                        }
                    }
                    if pane.exit_code.is_none() && output_drained {
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
        }
        backlog
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown
    }

    pub(crate) fn shutdown(&mut self) {
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
                    reason: "FACT closed".into(),
                });
        }
    }

    fn stats(&self) -> EngineStats {
        EngineStats {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: fact_protocol::PROTOCOL_VERSION,
            uptime_ms: self.started.elapsed().as_millis() as u64,
            live_panes: self.sessions.values().map(|s| s.panes.len()).sum(),
            client_count: self.subscribers.len(),
        }
    }

    fn create_session(
        &mut self,
        name: String,
        grid: u8,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let resolved = self
            .config
            .resolve_profile(profile)
            .map_err(map_config_error)?;
        let count = grid.clamp(1, MAX_GRID_PANES) as usize;
        let ids: Vec<u64> = (0..count).map(|_| self.alloc_pane_id()).collect();
        let layout = SessionLayout::new(1, &ids);
        let options = LayoutOptions {
            cols: DEFAULT_VIEWPORT_COLS,
            rows: DEFAULT_VIEWPORT_ROWS,
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
        // Only an explicit profile title is preserved verbatim. Automatic
        // labels are normalized from the resolved executable by spawn_pane.
        let profile_title = resolved.title.clone().unwrap_or_default();
        for id in &ids {
            let (cols, rows) = dims.get(id).copied().unwrap_or((80, 24));
            let spawned = spawn_pane(
                &self.config,
                self.waker.as_ref(),
                PaneSpec {
                    pane: *id,
                    profile,
                    cwd,
                    cols,
                    rows,
                    title_hint: &profile_title,
                },
            )?;
            let record = PaneRecord {
                id: *id,
                pid: spawned.pty.pid(),
                exit_code: None,
                profile: profile.map(str::to_string),
                cwd: spawned.cwd.clone(),
            };
            panes.insert(*id, spawned);
            records.push(record);
        }
        let data = SessionData::new(name.clone(), layout, records);
        let session = LiveSession {
            data,
            panes,
            viewport_cols: DEFAULT_VIEWPORT_COLS,
            viewport_rows: DEFAULT_VIEWPORT_ROWS,
            layout_epoch: 1,
            metadata_epoch: 1,
        };
        self.sessions.insert(key_of(&name), session);
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
            // Pasted text must not be able to end the paste early and have the
            // rest run as typed input.
            let normalized = normalized
                .replace(BRACKETED_PASTE_START, "")
                .replace(BRACKETED_PASTE_END, "");
            format!("{BRACKETED_PASTE_START}{normalized}{BRACKETED_PASTE_END}").into_bytes()
        } else {
            normalized.into_bytes()
        };
        live.pty
            .write_paste(&bytes)
            .map_err(|error| ProtocolError::invalid(error.to_string()))?;
        live.term.reset_scroll();
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
            rows: layout_rows(session),
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
        let spawned = spawn_pane(
            &self.config,
            self.waker.as_ref(),
            PaneSpec {
                pane: id,
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                title_hint: "",
            },
        )?;
        let pid = spawned.pty.pid();
        let pane_cwd = spawned.cwd.clone();
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
            pid,
            exit_code: None,
            profile,
            cwd: pane_cwd,
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
        let mut spawned = spawn_pane(
            &self.config,
            self.waker.as_ref(),
            PaneSpec {
                pane,
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                title_hint: &title,
            },
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
        let id = self.alloc_pane_id();
        let spawned = spawn_pane(
            &self.config,
            self.waker.as_ref(),
            PaneSpec {
                pane: id,
                profile: profile.as_deref(),
                cwd: cwd.as_deref(),
                cols,
                rows,
                title_hint: "",
            },
        )?;
        let title = spawned.display_title.clone();
        let pid = spawned.pty.pid();
        let pane_cwd = spawned.cwd.clone();
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
            pid,
            exit_code: None,
            profile,
            cwd: pane_cwd,
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

    fn resize_focused(
        &mut self,
        key: &str,
        direction: FocusDirection,
    ) -> Result<(), ProtocolError> {
        let direction = match direction {
            FocusDirection::Left => fact_layout::ResizeDirection::Left,
            FocusDirection::Right => fact_layout::ResizeDirection::Right,
            FocusDirection::Up => fact_layout::ResizeDirection::Up,
            FocusDirection::Down => fact_layout::ResizeDirection::Down,
        };
        let moved = self.edit_layout(key, |layout| {
            layout.resize_focused(direction, fact_layout::RESIZE_STEP)
        })?;
        if moved {
            Ok(())
        } else {
            Err(ProtocolError::invalid(
                "nothing to resize on that side of the focused pane",
            ))
        }
    }

    /// Apply a layout change. When `edit` reports a change, refresh pane
    /// sizes and every subscriber; a no-op costs no frame.
    fn edit_layout(
        &mut self,
        key: &str,
        edit: impl FnOnce(&mut SessionLayout) -> Result<bool, fact_layout::LayoutError>,
    ) -> Result<bool, ProtocolError> {
        let session = self
            .sessions
            .get_mut(key)
            .ok_or_else(|| ProtocolError::not_found("session is not running"))?;
        if !edit(&mut session.data.layout).map_err(map_layout_error)? {
            return Ok(false);
        }
        session.layout_epoch = session.layout_epoch.wrapping_add(1);
        session.metadata_epoch = session.metadata_epoch.wrapping_add(1);
        self.resize_session(key);
        Ok(true)
    }

    fn end_live_empty(&mut self, key: &str) {
        if let Some(session) = self.sessions.remove(key) {
            for (_, pane) in session.panes {
                pane.pty.kill();
                drop(pane);
            }
            self.end_subscribers(key, "all panes closed");
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

    /// Send each subscriber the frame it needs. Returns `true` when a full
    /// channel deferred a frame.
    fn pump_frames(&mut self) -> bool {
        let mut blocked = false;
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
                Err(TrySendError::Full(_)) => {
                    blocked = true;
                    index += 1;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.subscribers.remove(index);
                }
            }
        }
        blocked
    }

    fn build_frame_for(&mut self, index: usize) -> Option<BuiltFrame> {
        let key = self.subscribers.get(index)?.session_key.clone();
        let min_cols = self.config.min_pane_width;
        let min_rows = self.config.min_pane_height;
        let subscriber = self.subscribers.get_mut(index)?;
        let session = self.sessions.get_mut(&key)?;
        let layout = session.data.layout.layout(LayoutOptions {
            cols: session.viewport_cols,
            rows: layout_rows(session),
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
            seq: 0,
            session: session.data.summary(),
            tabs,
            active_tab,
            focus: layout.focused,
            panes: pane_views,
            minimized,
            splits,
            viewport: Viewport {
                cols: session.viewport_cols,
                rows: session.viewport_rows,
                slot_rows: layout_rows(session),
                slot_cols: session.viewport_cols,
            },
            tiny_mode: layout.tiny_mode,
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
            rows: layout_rows(session),
            min_content_cols: self.config.min_pane_width,
            min_content_rows: self.config.min_pane_height,
        }
    }

    fn alloc_pane_id(&mut self) -> u64 {
        let id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1).max(1);
        id
    }

    fn session_key(&self, _name: &str) -> Result<String, ProtocolError> {
        let key = key_of(SESSION_NAME);
        if self.sessions.contains_key(&key) {
            Ok(key)
        } else {
            Err(ProtocolError::not_found("FACT workspace is closed"))
        }
    }

    fn require_bound(&self, bound: Option<&str>) -> Result<String, ProtocolError> {
        self.session_key(bound.unwrap_or(SESSION_NAME))
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
    waker: Option<&OutputWaker>,
    spec: PaneSpec<'_>,
) -> Result<LivePane, ProtocolError> {
    let PaneSpec {
        pane,
        profile,
        cwd,
        cols,
        rows,
        title_hint,
    } = spec;
    let resolved = config.resolve_profile(profile).map_err(map_config_error)?;
    // Keep the requested directory even when it is currently missing. The
    // shell falls back for this launch, but the path survives so a later
    // restart can use it once the directory is available again.
    let requested_cwd = cwd
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            fact_config::expand_path(value).map_err(|error| {
                ProtocolError::invalid(format!("working directory is invalid: {error}"))
            })
        })
        .transpose()?
        .map(absolute_dir);
    let workdir = requested_cwd
        .clone()
        .filter(|dir| dir.is_dir())
        .or_else(|| resolved.cwd.clone().filter(|dir| dir.is_dir()))
        .map(absolute_dir);

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
    // Startup bursts of terminal queries must not drop replies.
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(256);
    sink.attach_writer(response_tx);
    let (foreground, background) = config.terminal_colors();
    sink.set_default_colors(foreground, background);
    let term = TerminalModel::new(cols, rows, config.scrollback_lines, sink);

    let mut options = SpawnOptions::new(resolved.command.clone(), cols, rows);
    options.args = resolved.args.clone();
    options.cwd = workdir.clone();
    options.env = fact_config::pane_environment(&resolved);
    options
        .env
        .insert(fact_protocol::ENV_PANE.to_string(), pane.to_string());
    options.waker = waker.cloned();
    let pty = PtyProcess::spawn(options)
        .map_err(|error| ProtocolError::internal(format!("could not start pane: {error}")))?;

    // Persist the explicitly requested directory even if it is missing, but
    // fall back to the effective profile directory when the request omitted one.
    let record_cwd = requested_cwd.or_else(|| workdir.clone());
    Ok(LivePane {
        pty,
        term,
        responses: response_rx,
        version: 1,
        default_title: title.clone(),
        display_title: title,
        last_bell: 0,
        exit_code: None,
        cwd: record_cwd.as_ref().map(|path| path.display().to_string()),
    })
}

fn absolute_dir(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn exit_notice_follows_all_queued_process_output() {
        let mut config = Config {
            default_profile: "tail-output".into(),
            ..Config::default()
        };
        config.profiles.insert("tail-output".into(), fact_config::Profile {
            enabled: true,
            command: Some(format!("{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()))),
            args: vec!["-NoLogo".into(), "-NoProfile".into(), "-Command".into(),
                "1..90 | ForEach-Object { [Console]::Write(\"OUTPUT_$_`r`n\"); Start-Sleep -Milliseconds 20 }; [Console]::Write('FACT_FINAL_OUTPUT')".into()],
            ..fact_config::Profile::default()
        });
        let mut manager = SessionManager::new(config, 1, Some("tail-output"), None, None).unwrap();
        let key = key_of(SESSION_NAME);
        let pane_id = *manager.sessions[&key].panes.keys().next().unwrap();
        // Delay the owner tick until the process has exited and its output is
        // queued, reproducing a busy pane with more than one tick of output.
        let pty = &manager.sessions[&key].panes[&pane_id].pty;
        assert_eq!(pty.wait_for_exit(Duration::from_secs(15)), Some(0));
        std::thread::sleep(Duration::from_millis(300));
        let deadline = Instant::now() + Duration::from_secs(10);
        while manager.sessions[&key].panes[&pane_id].exit_code.is_none() {
            manager.tick();
            assert!(Instant::now() < deadline, "output did not finish draining");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pane = &manager.sessions[&key].panes[&pane_id];
        let text = pane.term.plain_text();
        let tail = text
            .find("FACT_FINAL_OUTPUT")
            .expect("final output is present");
        let notice = text
            .find("[FACT] process exited")
            .expect("exit notice is present");
        assert!(
            notice > tail,
            "exit notice preceded the final output: {text}"
        );
    }
}
