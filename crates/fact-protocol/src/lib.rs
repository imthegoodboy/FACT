//! Versioned wire protocol shared by the FACT client, CLI and daemon.
//!
//! Frames are serialized with `bincode` and length-prefixed by `fact-ipc`.
//! The protocol is intentionally transport-independent so it can be fuzzed and
//! exercised without spawning a daemon.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_WIRE_FRAME: usize = 64 * 1024 * 1024;
pub const MAX_INPUT_CHUNK: usize = 256 * 1024;
pub const MAX_PASTE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SESSIONS: usize = 128;
pub const MAX_GRID_PANES: u8 = 32;
pub const MAX_SEARCH_QUERY: usize = 256;

/// Cell drawing flags. Kept as a plain bitfield for protocol stability.
pub mod cell_flags {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const DIM: u16 = 1 << 5;
    pub const STRIKEOUT: u16 = 1 << 6;
    pub const HIDDEN: u16 = 1 << 7;
    pub const WIDE_CHAR: u16 = 1 << 8;
    pub const WIDE_SPACER: u16 = 1 << 9;
}

/// Pane terminal modes needed by the input layer.
pub mod pane_modes {
    pub const SHOW_CURSOR: u32 = 1 << 0;
    pub const APP_CURSOR: u32 = 1 << 1;
    pub const APP_KEYPAD: u32 = 1 << 2;
    pub const ALT_SCREEN: u32 = 1 << 3;
    pub const BRACKETED_PASTE: u32 = 1 << 4;
    pub const MOUSE_CLICK: u32 = 1 << 5;
    pub const MOUSE_DRAG: u32 = 1 << 6;
    pub const MOUSE_MOTION: u32 = 1 << 7;
    pub const SGR_MOUSE: u32 = 1 << 8;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u16,
    pub client_version: String,
    /// Random per-daemon authentication token read from the user-only runtime file.
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello(Hello),
    Request { id: u64, command: Command },
    Subscribe { token: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    HelloOk {
        protocol: u16,
        daemon_version: String,
        daemon_pid: u32,
    },
    RequestOk {
        id: u64,
        payload: ResponsePayload,
    },
    RequestErr {
        id: u64,
        error: ProtocolError,
    },
    Frame(Box<Frame>),
    SessionEnded {
        reason: String,
    },
    Fatal(ProtocolError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Ping,
    Stats,
    ListSessions {
        include_archived: bool,
    },
    /// Create, warm-restore or attach to a session. With `name: None`, resumes the
    /// most recent live session, otherwise the most recent saved session, otherwise
    /// creates `default`.
    ResumeSession {
        name: Option<String>,
        grid: Option<u8>,
        profile: Option<String>,
        cwd: Option<String>,
    },
    Attach {
        name: String,
    },
    RenameSession {
        name: String,
        new_name: String,
    },
    SaveSession {
        name: String,
    },
    ArchiveSession {
        name: String,
    },
    UnarchiveSession {
        name: String,
    },
    DeleteSession {
        name: String,
        force: bool,
    },
    TerminateSession {
        name: String,
    },
    ExportSession {
        name: String,
    },
    ImportSession {
        json: String,
        name: Option<String>,
        overwrite: bool,
    },
    /// Ask attached FACT UIs on a session to detach cleanly.
    DetachClients {
        name: Option<String>,
    },
    ShutdownDaemon,
    Input {
        pane: u64,
        data: Vec<u8>,
    },
    /// Clipboard paste. The daemon decides whether to wrap in bracketed paste.
    Paste {
        pane: u64,
        text: String,
    },
    ResizeClient {
        cols: u16,
        rows: u16,
    },
    SplitPane {
        pane: u64,
        direction: SplitDirection,
    },
    ClosePane {
        pane: u64,
    },
    RestartPane {
        pane: u64,
    },
    FocusPane {
        pane: u64,
    },
    FocusDirection {
        direction: FocusDirection,
    },
    NextPane,
    PrevPane,
    ZoomPane {
        zoomed: bool,
    },
    MinimizePane {
        pane: u64,
    },
    RestorePanes,
    NewTab {
        profile: Option<String>,
        cwd: Option<String>,
    },
    CloseTab {
        tab: u32,
    },
    SelectTab {
        tab: u32,
    },
    RenameTab {
        tab: u32,
        title: String,
    },
    NextTab,
    PrevTab,
    MovePaneToTab {
        pane: u64,
        target_tab: u32,
    },
    SwapPanes {
        a: u64,
        b: u64,
    },
    ScrollPane {
        pane: u64,
        delta: i32,
    },
    ScrollToLine {
        pane: u64,
        line: i32,
    },
    SearchPane {
        pane: u64,
        query: String,
        case_sensitive: bool,
        limit: u16,
    },
    DragSplit {
        path: Vec<u8>,
        ratio: f32,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponsePayload {
    Pong {
        daemon_pid: u32,
        uptime_ms: u64,
        live_sessions: usize,
    },
    Stats(DaemonStats),
    Sessions(Vec<SessionSummary>),
    Attached(Box<AttachInfo>),
    Session(SessionSummary),
    Ack,
    Export {
        filename: String,
        json: String,
    },
    Imported(SessionSummary),
    Search(SearchResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachInfo {
    pub session: SessionSummary,
    /// One-time token used by the separate events pipe.
    pub stream_token: String,
    pub cold_recovered: bool,
    pub recovery_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStats {
    pub daemon_pid: u32,
    pub version: String,
    pub protocol: u16,
    pub uptime_ms: u64,
    pub live_sessions: usize,
    pub live_panes: usize,
    pub client_count: usize,
    pub database_path: String,
    pub memory_bytes: Option<u64>,
    pub cpu_time_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub archived: bool,
    pub live: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub cold_recovered_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub protocol: u16,
    pub seq: u64,
    pub session: SessionSummary,
    pub tabs: Vec<TabView>,
    pub active_tab: u32,
    pub focus: Option<u64>,
    pub panes: Vec<PaneView>,
    pub minimized: Vec<PaneSummary>,
    pub splits: Vec<SplitHandle>,
    pub viewport: Viewport,
    pub tiny_mode: bool,
    pub detach_seq: u64,
    pub server_time_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewport {
    pub cols: u16,
    pub rows: u16,
    pub slot_rows: u16,
    pub slot_cols: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabView {
    pub id: u32,
    pub title: String,
    pub active: bool,
    pub pane_count: usize,
    pub zoomed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: u64,
    pub title: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneView {
    pub id: u64,
    pub tab: u32,
    pub title: String,
    pub pid: u32,
    pub alive: bool,
    pub exit_code: Option<i32>,
    /// Slot including the one-line pane title bar.
    pub slot: RectDto,
    pub content_cols: u16,
    pub content_rows: u16,
    /// Incremented only when terminal cells changed. Cells are omitted from a
    /// frame when the stream already sent this version for this pane.
    pub version: u64,
    pub cells: Option<Vec<CellDto>>,
    pub cursor: CursorDto,
    pub modes: u32,
    pub scroll_offset: usize,
    pub history_lines: usize,
    pub bell_seq: u64,
    pub cold_restored: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellDto {
    pub ch: char,
    pub zero: Vec<char>,
    pub fg: ColorDto,
    pub bg: ColorDto,
    pub flags: u16,
}

impl CellDto {
    pub fn blank(fg: ColorDto, bg: ColorDto) -> Self {
        Self {
            ch: ' ',
            zero: Vec::new(),
            fg,
            bg,
            flags: 0,
        }
    }

    pub fn symbol(&self) -> String {
        if self.zero.is_empty() {
            self.ch.to_string()
        } else {
            let mut s = String::with_capacity(4 + self.zero.len() * 3);
            s.push(self.ch);
            s.extend(self.zero.iter());
            s
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ColorDto {
    /// ANSI 0..=15, or one of the semantic slots above 15.
    Named(u8),
    Indexed(u8),
    Rgb {
        r: u8,
        g: u8,
        b: u8,
    },
}

pub mod semantic_color {
    pub const BLACK: u8 = 0;
    pub const RED: u8 = 1;
    pub const GREEN: u8 = 2;
    pub const YELLOW: u8 = 3;
    pub const BLUE: u8 = 4;
    pub const MAGENTA: u8 = 5;
    pub const CYAN: u8 = 6;
    pub const WHITE: u8 = 7;
    pub const BRIGHT_BLACK: u8 = 8;
    pub const BRIGHT_RED: u8 = 9;
    pub const BRIGHT_GREEN: u8 = 10;
    pub const BRIGHT_YELLOW: u8 = 11;
    pub const BRIGHT_BLUE: u8 = 12;
    pub const BRIGHT_MAGENTA: u8 = 13;
    pub const BRIGHT_CYAN: u8 = 14;
    pub const BRIGHT_WHITE: u8 = 15;
    pub const FOREGROUND: u8 = 16;
    pub const BACKGROUND: u8 = 17;
    pub const CURSOR: u8 = 18;
    pub const BRIGHT_FOREGROUND: u8 = 19;
    pub const DIM_BACKGROUND: u8 = 20;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorDto {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RectDto {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl RectDto {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x.saturating_add(self.width)
            && y < self.y.saturating_add(self.height)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitHandle {
    /// Binary tree path: 0 = first child, 1 = second child.
    pub path: Vec<u8>,
    pub orientation: SplitOrientation,
    /// The whole region owned by this split node, in content coordinates.
    pub region: RectDto,
    pub ratio: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SplitOrientation {
    /// Children are side by side; the gutter is vertical.
    Horizontal,
    /// Children are stacked; the gutter is horizontal.
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub pane: u64,
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub line: i32,
    pub column: u16,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorCode {
    Unauthorized,
    VersionMismatch,
    NotFound,
    Conflict,
    Invalid,
    Backpressure,
    Busy,
    Io,
    Internal,
    Unsupported,
}

/// Validated session names are useful at every boundary.
pub fn validate_session_name(name: &str) -> Result<(), ProtocolError> {
    let trimmed = name.trim();
    if trimmed != name {
        return Err(ProtocolError::invalid(
            "session name may not begin or end with whitespace",
        ));
    }
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(ProtocolError::invalid(
            "session name must contain 1..=64 characters",
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ProtocolError::invalid(
            "session name may only contain ASCII letters, digits, '-', '_' and '.'",
        ));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(ProtocolError::invalid("session name is reserved"));
    }
    Ok(())
}

/// Remove control characters from terminal-provided titles before displaying or
/// broadcasting them.
pub fn sanitize_title(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if out.chars().count() >= 80 {
            break;
        }
        if c.is_control() {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "Shell".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_strict() {
        assert!(validate_session_name("dev").is_ok());
        assert!(validate_session_name("dev-2.alpha_1").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("../escape").is_err());
        assert!(validate_session_name("a b").is_err());
        assert!(validate_session_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn titles_are_sanitized() {
        assert_eq!(sanitize_title("hello\u{7}\r\nworld"), "helloworld");
        assert_eq!(sanitize_title("\u{1b}[31mred"), "[31mred");
        assert_eq!(sanitize_title("   "), "Shell");
        assert!(sanitize_title(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn zero_width_symbols_are_preserved() {
        let cell = CellDto {
            ch: 'e',
            zero: vec!['\u{301}'],
            fg: ColorDto::Named(16),
            bg: ColorDto::Named(17),
            flags: 0,
        };
        assert_eq!(cell.symbol(), "e\u{301}");
    }

    #[test]
    fn rect_contains() {
        let r = RectDto {
            x: 3,
            y: 2,
            width: 4,
            height: 5,
        };
        assert!(r.contains(3, 2));
        assert!(r.contains(6, 6));
        assert!(!r.contains(7, 6));
        assert!(!r.contains(2, 2));
    }
}
