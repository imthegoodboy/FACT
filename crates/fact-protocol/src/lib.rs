//! Message types shared by the FACT engine and terminal UI.
//!
//! Everything stays inside one process: commands travel over a channel and
//! frames are handed back as plain values. The types keep serde derives so they
//! remain easy to snapshot and test, but nothing is written to disk or sent over
//! a network.

use serde::{Deserialize, Serialize};

/// Bumped when command or frame semantics change. It identifies the message
/// shape used inside this build and in tests.
pub const PROTOCOL_VERSION: u16 = 7;
pub const MAX_INPUT_CHUNK: usize = 256 * 1024;
pub const MAX_PASTE_BYTES: usize = 4 * 1024 * 1024;
pub const BRACKETED_PASTE_START: &str = "\x1b[200~";
pub const BRACKETED_PASTE_END: &str = "\x1b[201~";
pub const MAX_GRID_PANES: u8 = 32;
pub const MAX_SEARCH_QUERY: usize = 256;

/// Environment value injected into every shell so scripts can identify their
/// FACT pane. It is informational; FACT itself never reads it back.
pub const ENV_PANE: &str = "FACT_PANE";

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

/// Events pushed from the engine to the terminal UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    Frame(Box<Frame>),
    SessionEnded { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Ping,
    Stats,
    /// Stop the engine and every shell it owns.
    Shutdown,
    Input {
        pane: u64,
        data: Vec<u8>,
    },
    /// Clipboard paste. The engine wraps it in bracketed paste when the pane
    /// has enabled that mode.
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
    /// Set the scrollback offset directly (0 = live output).
    SetScrollOffset {
        pane: u64,
        offset: usize,
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
    Pong { pid: u32, uptime_ms: u64 },
    Stats(EngineStats),
    Ack,
    Search(SearchResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStats {
    pub pid: u32,
    pub version: String,
    pub protocol: u16,
    pub uptime_ms: u64,
    pub live_panes: usize,
    pub client_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub pane_count: usize,
    pub tab_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
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
    /// Slot occupied by the pane. Terminal content starts at the slot origin.
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

/// Turn the noisy title a shell publishes (often a full executable path) into a
/// short label that is safe to show on the tab strip.
pub fn friendly_title(input: &str) -> String {
    // Strip the path before applying the 80-character display cap: Windows
    // shell titles routinely start with a long WindowsApps path ending in the
    // actual executable name.
    let cleaned: String = input.chars().filter(|c| !c.is_control()).collect();
    let text = cleaned.trim();
    let base = text.rsplit(['\\', '/']).next().unwrap_or(text).trim();
    let base = base
        .get(base.len().saturating_sub(4)..)
        .filter(|suffix| suffix.eq_ignore_ascii_case(".exe"))
        .map(|_| &base[..base.len() - 4])
        .unwrap_or(base);
    let lower = base.to_ascii_lowercase();
    let label = match lower.as_str() {
        "pwsh" | "powershell" | "powershell_ise" => "PowerShell".to_string(),
        "cmd" | "command" => "Command Prompt".to_string(),
        "wsl" | "wslhost" => "WSL".to_string(),
        "bash" => "Bash".to_string(),
        "zsh" => "Zsh".to_string(),
        "fish" => "Fish".to_string(),
        "nu" | "nushell" => "Nushell".to_string(),
        "" => "Shell".to_string(),
        _ => base.to_string(),
    };
    sanitize_title(&label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_sanitized() {
        assert_eq!(sanitize_title("hello\u{7}\r\nworld"), "helloworld");
        assert_eq!(sanitize_title("\u{1b}[31mred"), "[31mred");
        assert_eq!(sanitize_title("   "), "Shell");
        assert!(sanitize_title(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn shell_paths_become_friendly_titles() {
        assert_eq!(
            friendly_title(
                r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.6.7.0_x64__8wekyb3d8bbwe\pwsh.exe"
            ),
            "PowerShell"
        );
        assert_eq!(
            friendly_title(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "PowerShell"
        );
        assert_eq!(
            friendly_title(r"C:\Windows\System32\cmd.exe"),
            "Command Prompt"
        );
        assert_eq!(friendly_title("pwsh"), "PowerShell");
        assert_eq!(friendly_title("claude"), "claude");
        assert_eq!(friendly_title("日本語"), "日本語");
        assert_eq!(friendly_title("   "), "Shell");
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
