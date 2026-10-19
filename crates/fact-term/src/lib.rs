//! Terminal emulation wrapper.
//!
//! Alacritty's mature VT parser/model handles ANSI, alternate screen, wide
//! characters and scrollback. This crate adds the bounded IPC snapshot format,
//! safe event handling and cold-recovery serialization used by FACT.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::Rgb;
use alacritty_terminal::vte::ansi::{
    Color as AColor, CursorShape as TermCursorShape, NamedColor, Processor,
};
use fact_protocol::{
    cell_flags, pane_modes, semantic_color, CellDto, ColorDto, CursorDto, CursorShape,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
pub const MAX_RESTORE_BYTES: usize = 4 * 1024 * 1024;

/// Shared state filled by parser callbacks and consumed by the daemon.
#[derive(Default)]
struct EventState {
    title: String,
    response_tx: Option<SyncSender<Vec<u8>>>,
    bell_seq: u64,
    last_error: Option<String>,
}

use std::sync::mpsc::SyncSender;

#[derive(Clone, Default)]
pub struct TerminalEventSink {
    state: Arc<Mutex<EventState>>,
    wakeup: Arc<AtomicU64>,
    dimensions: Arc<Mutex<(u16, u16)>>,
}

impl TerminalEventSink {
    pub fn new() -> Self {
        Self {
            dimensions: Arc::new(Mutex::new((100, 30))),
            ..Self::default()
        }
    }

    pub fn set_dimensions(&self, cols: u16, rows: u16) {
        if let Ok(mut dims) = self.dimensions.lock() {
            *dims = (cols.max(1), rows.max(1));
        }
    }

    fn dimensions(&self) -> (u16, u16) {
        self.dimensions.lock().map(|d| *d).unwrap_or((100, 30))
    }

    pub fn attach_writer(&self, tx: SyncSender<Vec<u8>>) {
        if let Ok(mut state) = self.state.lock() {
            state.response_tx = Some(tx);
        }
    }

    pub fn title(&self) -> String {
        self.state
            .lock()
            .map(|s| s.title.clone())
            .unwrap_or_default()
    }

    pub fn bell_seq(&self) -> u64 {
        self.state.lock().map(|s| s.bell_seq).unwrap_or(0)
    }

    pub fn wakeup_seq(&self) -> u64 {
        self.wakeup.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.last_error.clone())
    }

    fn set_error(&self, message: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.last_error = Some(message.into());
        }
    }

    fn send_pty(&self, bytes: Vec<u8>) {
        let tx = self.state.lock().ok().and_then(|s| s.response_tx.clone());
        if let Some(tx) = tx {
            // Never block the parser on a full input queue.
            let _ = tx.try_send(bytes);
        }
    }
}

struct Listener {
    sink: TerminalEventSink,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                if let Ok(mut state) = self.sink.state.lock() {
                    state.title = title;
                }
            }
            Event::ResetTitle => {
                if let Ok(mut state) = self.sink.state.lock() {
                    state.title.clear();
                }
            }
            Event::Bell => {
                if let Ok(mut state) = self.sink.state.lock() {
                    state.bell_seq = state.bell_seq.wrapping_add(1);
                }
                self.sink.wakeup.fetch_add(1, Ordering::Relaxed);
            }
            Event::PtyWrite(text) => {
                self.sink.send_pty(text.into_bytes());
            }
            Event::ColorRequest(index, format) => {
                // Report a sane xterm 256-color palette. Applications query these
                // during startup; returning an empty response breaks some TUIs.
                let rgb = palette_rgb(index);
                let text = format(rgb);
                self.sink.send_pty(text.into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let (cols, rows) = self.sink.dimensions();
                let text = format(alacritty_terminal::event::WindowSize {
                    num_lines: rows,
                    num_cols: cols,
                    cell_width: 8,
                    cell_height: 16,
                });
                self.sink.send_pty(text.into_bytes());
            }
            Event::ClipboardStore(_, _) | Event::ClipboardLoad(_, _) => {
                // OSC 52 is disabled in the terminal config, but if a future
                // alacritty version still emits these events we must never touch
                // the host clipboard from the daemon.
            }
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.sink.wakeup.fetch_add(1, Ordering::Relaxed);
            }
            Event::Exit | Event::ChildExit(_) => {}
        }
    }
}

pub struct TerminalModel {
    term: Term<Listener>,
    parser: Processor,
    sink: TerminalEventSink,
    cols: u16,
    rows: u16,
    cold_restored: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    pub cells: Vec<CellDto>,
    pub cursor: CursorDto,
    pub modes: u32,
    pub scroll_offset: usize,
    pub history_lines: usize,
    pub title: String,
    pub bell_seq: u64,
}

#[derive(Debug, Clone)]
pub struct RecoverySnapshot {
    pub ansi: String,
    pub cols: u16,
    pub rows: u16,
    pub captured_at_ms: i64,
    pub sha256: String,
    pub format_version: u32,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SearchHitInternal {
    pub line: i32,
    pub column: u16,
    pub preview: String,
}

impl TerminalModel {
    pub fn new(cols: u16, rows: u16, history: usize, sink: TerminalEventSink) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let config = TermConfig {
            scrolling_history: history,
            kitty_keyboard: false,
            ..Default::default()
        };
        sink.set_dimensions(cols, rows);
        let listener = Listener { sink: sink.clone() };
        let term = Term::new(
            config,
            &TermSize {
                cols: cols as usize,
                lines: rows as usize,
            },
            listener,
        );
        Self {
            term,
            parser: Processor::new(),
            sink,
            cols,
            rows,
            cold_restored: false,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn history_lines(&self) -> usize {
        self.term.grid().history_size()
    }

    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn cold_restored(&self) -> bool {
        self.cold_restored
    }

    pub fn mark_cold_restored(&mut self) {
        self.cold_restored = true;
    }

    /// Feed bytes from the ConPTY. Parser panics are contained so a malformed
    /// escape sequence can never take down the daemon.
    pub fn feed(&mut self, bytes: &[u8]) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.advance(&mut self.term, bytes);
        }));
        if result.is_err() {
            self.sink
                .set_error("terminal parser panic contained; pane may be unstable");
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.sink.set_dimensions(cols, rows);
        self.term.resize(TermSize {
            cols: cols as usize,
            lines: rows as usize,
        });
    }

    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_page(&mut self, pages: i32) {
        for _ in 0..pages.unsigned_abs() {
            self.term.scroll_display(if pages > 0 {
                Scroll::PageUp
            } else {
                Scroll::PageDown
            });
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Scroll so that a grid line (negative = history) is visible.
    pub fn scroll_to_line(&mut self, line: i32) {
        let history = self.history_lines() as i32;
        let target = (-line).clamp(0, history.max(0));
        let current = self.scroll_offset() as i32;
        self.term.scroll_display(Scroll::Delta(target - current));
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        let current = self.scroll_offset() as i32;
        let target = offset.min(self.history_lines()) as i32;
        self.term.scroll_display(Scroll::Delta(target - current));
    }

    pub fn reset_scroll(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn mouse_mode(&self) -> u32 {
        let mode = *self.term.mode();
        let mut out = 0;
        if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            out |= pane_modes::MOUSE_CLICK;
        }
        if mode.contains(TermMode::MOUSE_DRAG) {
            out |= pane_modes::MOUSE_DRAG;
        }
        if mode.contains(TermMode::MOUSE_MOTION) {
            out |= pane_modes::MOUSE_MOTION;
        }
        if mode.contains(TermMode::SGR_MOUSE) {
            out |= pane_modes::SGR_MOUSE;
        }
        out
    }

    pub fn modes(&self) -> u32 {
        let mode = *self.term.mode();
        let mut out = self.mouse_mode();
        if mode.contains(TermMode::SHOW_CURSOR) {
            out |= pane_modes::SHOW_CURSOR;
        }
        if mode.contains(TermMode::APP_CURSOR) {
            out |= pane_modes::APP_CURSOR;
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            out |= pane_modes::APP_KEYPAD;
        }
        if mode.contains(TermMode::ALT_SCREEN) {
            out |= pane_modes::ALT_SCREEN;
        }
        if mode.contains(TermMode::BRACKETED_PASTE) {
            out |= pane_modes::BRACKETED_PASTE;
        }
        out
    }

    pub fn title(&self) -> String {
        self.sink.title()
    }

    pub fn bell_seq(&self) -> u64 {
        self.sink.bell_seq()
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        let render = self.term.renderable_content();
        let mut cells: Vec<CellDto> = Vec::with_capacity(self.cols as usize * self.rows as usize);
        for indexed in render.display_iter {
            let cell = indexed.cell;
            let flags = convert_flags(cell.flags);
            let zero: Vec<char> = cell
                .zerowidth()
                .map(|chars| chars.iter().take(8).copied().collect())
                .unwrap_or_default();
            cells.push(CellDto {
                ch: if cell.c == '\0' { ' ' } else { cell.c },
                zero,
                fg: convert_color(cell.fg),
                bg: convert_color(cell.bg),
                flags,
            });
        }
        // Defensive: the iterator is expected to yield exactly cols*rows cells.
        let expected = self.cols as usize * self.rows as usize;
        if cells.len() < expected {
            cells.resize(
                expected,
                CellDto::blank(
                    ColorDto::Named(semantic_color::FOREGROUND),
                    ColorDto::Named(semantic_color::BACKGROUND),
                ),
            );
        } else if cells.len() > expected {
            cells.truncate(expected);
        }
        let point = render.cursor.point;
        let visible = render.cursor.shape != TermCursorShape::Hidden;
        let shape = match render.cursor.shape {
            TermCursorShape::Block => CursorShape::Block,
            TermCursorShape::Underline => CursorShape::Underline,
            TermCursorShape::Beam => CursorShape::Beam,
            TermCursorShape::HollowBlock => CursorShape::Block,
            TermCursorShape::Hidden => CursorShape::Hidden,
        };
        TerminalSnapshot {
            cells,
            cursor: CursorDto {
                x: point.column.0 as u16,
                y: point.line.0.clamp(0, u16::MAX as i32) as u16,
                visible,
                shape,
            },
            modes: self.modes(),
            scroll_offset: render.display_offset,
            history_lines: self.history_lines(),
            title: self.title(),
            bell_seq: self.sink.bell_seq(),
        }
    }

    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        let top = -(self.term.grid().history_size() as i32);
        let bottom = self.rows as i32;
        for line in top..bottom {
            let mut row = String::new();
            for col in 0..self.cols as usize {
                let cell = &self.term.grid()[Line(line)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.c != '\0' {
                    row.push(cell.c);
                    if let Some(zero) = cell.zerowidth() {
                        for c in zero.iter().take(8) {
                            row.push(*c);
                        }
                    }
                }
            }
            out.push_str(row.trim_end());
            out.push_str("\r\n");
        }
        out
    }

    /// Serialize the visual buffer to ANSI SGR text for cold recovery. Bounded
    /// by both line count and byte size so a terminal full of output cannot
    /// create a huge or slow-to-restore snapshot.
    pub fn recovery_snapshot(&self, max_bytes: usize, captured_at_ms: i64) -> RecoverySnapshot {
        let max_bytes = max_bytes.clamp(4096, MAX_RESTORE_BYTES);
        let history = self.term.grid().history_size() as i32;
        let bottom = self.rows as i32;
        let top = -(history);
        let mut lines: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut line = bottom - 1;
        // Walk backwards from the visible bottom so truncation keeps the newest
        // output, then reverse for chronological replay.
        while line >= top {
            let mut row = String::new();
            let mut active = StyleState::default();
            let mut last_non_space = 0usize;
            let mut columns_seen = 0usize;
            for col in 0..self.cols as usize {
                let cell = &self.term.grid()[Line(line)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                columns_seen = col + 1;
                let style = StyleState::from_cell(cell);
                if style != active {
                    row.push_str(&style.sgr_from(&active));
                    active = style;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                row.push(ch);
                if let Some(zero) = cell.zerowidth() {
                    for c in zero.iter().take(8) {
                        row.push(*c);
                    }
                }
                if ch != ' ' || cell.zerowidth().is_some_and(|zero| !zero.is_empty()) {
                    last_non_space = row.len();
                }
                if row.len() >= max_bytes / 8 {
                    truncated = true;
                    break;
                }
            }
            let _ = columns_seen;
            row.truncate(last_non_space);
            if !active.is_default() {
                row.push_str("\x1b[0m");
            }
            bytes += row.len() + 2;
            if bytes > max_bytes.saturating_sub(2048) {
                truncated = true;
                break;
            }
            lines.push(row);
            line -= 1;
        }
        lines.reverse();
        // Blank rows below the last visible content must not be replayed: each
        // one would scroll the real content off the top of the restored screen.
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let mut ansi = lines.join("\r\n");
        if truncated {
            ansi.insert_str(
                0,
                "\x1b[2m[FACT: older output omitted from recovery snapshot]\x1b[0m\r\n",
            );
        }
        let mut hasher = Sha256::new();
        hasher.update(ansi.as_bytes());
        let sha256 = hex_encode(&hasher.finalize());
        RecoverySnapshot {
            ansi,
            cols: self.cols,
            rows: self.rows,
            captured_at_ms,
            sha256,
            format_version: SNAPSHOT_FORMAT_VERSION,
            truncated,
        }
    }

    /// Replay a validated cold-recovery snapshot. The caller displays a banner
    /// making it explicit that the original processes are gone.
    pub fn restore_snapshot(&mut self, ansi: &str) -> Result<(), RestoreError> {
        if ansi.len() > MAX_RESTORE_BYTES {
            return Err(RestoreError::TooLarge);
        }
        if ansi.chars().any(|c| c == '\0') {
            return Err(RestoreError::Invalid("snapshot contains NUL".into()));
        }
        self.feed(ansi.as_bytes());
        // Reset styling without moving the cursor: an extra newline on the
        // last screen row would scroll restored content off the top.
        self.feed(b"\x1b[0m");
        self.mark_cold_restored();
        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        case_sensitive: bool,
        limit: usize,
    ) -> (Vec<SearchHitInternal>, bool) {
        if query.is_empty() {
            return (Vec::new(), false);
        }
        let history = self.term.grid().history_size() as i32;
        let top = -history;
        // Visible lines are 0..rows; `Line(rows)` would be out of bounds.
        let bottom = self.rows as i32;
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let mut hits = Vec::new();
        let mut truncated = false;
        for line in top..bottom {
            let mut text = String::new();
            for col in 0..self.cols as usize {
                let cell = &self.term.grid()[Line(line)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.c != '\0' {
                    text.push(cell.c);
                }
            }
            let haystack = if case_sensitive {
                text.clone()
            } else {
                text.to_lowercase()
            };
            let mut start = 0usize;
            while let Some(pos) = haystack[start..].find(&needle) {
                let absolute = start + pos;
                if hits.len() >= limit {
                    truncated = true;
                    break;
                }
                let col = text
                    .char_indices()
                    .take_while(|(i, _)| *i < absolute)
                    .count() as u16;
                let preview = text.trim().chars().take(120).collect::<String>();
                hits.push(SearchHitInternal {
                    line,
                    column: col,
                    preview,
                });
                start = absolute + needle.len().max(1);
                if start >= haystack.len() {
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        (hits, truncated)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("recovery snapshot is too large")]
    TooLarge,
    #[error("recovery snapshot is invalid: {0}")]
    Invalid(String),
}

struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StyleState {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strike: bool,
}

impl StyleState {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn from_cell(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        let fg = color_key(cell.fg);
        let bg = color_key(cell.bg);
        Self {
            fg,
            bg,
            bold: cell.flags.contains(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            inverse: cell.flags.contains(Flags::INVERSE),
            strike: cell.flags.contains(Flags::STRIKEOUT),
        }
    }

    fn sgr_from(&self, _previous: &Self) -> String {
        let mut out = String::from("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.dim {
            out.push_str(";2");
        }
        if self.italic {
            out.push_str(";3");
        }
        if self.underline {
            out.push_str(";4");
        }
        if self.inverse {
            out.push_str(";7");
        }
        if self.strike {
            out.push_str(";9");
        }
        if let Some(fg) = self.fg {
            if fg & 0xFF00_0000 != 0 {
                let idx = fg & 0xFF;
                out.push_str(&format!(";38;5;{idx}"));
            } else {
                let r = (fg >> 16) & 0xFF;
                let g = (fg >> 8) & 0xFF;
                let b = fg & 0xFF;
                out.push_str(&format!(";38;2;{r};{g};{b}"));
            }
        }
        if let Some(bg) = self.bg {
            if bg & 0xFF00_0000 != 0 {
                let idx = bg & 0xFF;
                out.push_str(&format!(";48;5;{idx}"));
            } else {
                let r = (bg >> 16) & 0xFF;
                let g = (bg >> 8) & 0xFF;
                let b = bg & 0xFF;
                out.push_str(&format!(";48;2;{r};{g};{b}"));
            }
        }
        out.push('m');
        out
    }
}

fn color_key(color: AColor) -> Option<u32> {
    match color {
        AColor::Named(NamedColor::Foreground)
        | AColor::Named(NamedColor::Background)
        | AColor::Named(NamedColor::Cursor) => None,
        AColor::Named(named) => {
            let index = named_index(named);
            Some(0x0100_0000 | u32::from(index))
        }
        AColor::Indexed(index) => Some(0x0100_0000 | u32::from(index)),
        AColor::Spec(Rgb { r, g, b }) => {
            Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
        }
    }
}

fn convert_flags(flags: Flags) -> u16 {
    let mut out = 0;
    if flags.contains(Flags::BOLD) {
        out |= cell_flags::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        out |= cell_flags::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        out |= cell_flags::UNDERLINE;
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        out |= cell_flags::DOUBLE_UNDERLINE;
    }
    if flags.contains(Flags::INVERSE) {
        out |= cell_flags::INVERSE;
    }
    if flags.contains(Flags::DIM) {
        out |= cell_flags::DIM;
    }
    if flags.contains(Flags::STRIKEOUT) {
        out |= cell_flags::STRIKEOUT;
    }
    if flags.contains(Flags::HIDDEN) {
        out |= cell_flags::HIDDEN;
    }
    if flags.contains(Flags::WIDE_CHAR) {
        out |= cell_flags::WIDE_CHAR;
    }
    if flags.contains(Flags::WIDE_CHAR_SPACER) {
        out |= cell_flags::WIDE_SPACER;
    }
    out
}

fn convert_color(color: AColor) -> ColorDto {
    match color {
        AColor::Named(named) => ColorDto::Named(named_index(named)),
        AColor::Indexed(index) => ColorDto::Indexed(index),
        AColor::Spec(Rgb { r, g, b }) => ColorDto::Rgb { r, g, b },
    }
}

fn named_index(named: NamedColor) -> u8 {
    match named {
        NamedColor::Black => semantic_color::BLACK,
        NamedColor::Red => semantic_color::RED,
        NamedColor::Green => semantic_color::GREEN,
        NamedColor::Yellow => semantic_color::YELLOW,
        NamedColor::Blue => semantic_color::BLUE,
        NamedColor::Magenta => semantic_color::MAGENTA,
        NamedColor::Cyan => semantic_color::CYAN,
        NamedColor::White => semantic_color::WHITE,
        NamedColor::BrightBlack => semantic_color::BRIGHT_BLACK,
        NamedColor::BrightRed => semantic_color::BRIGHT_RED,
        NamedColor::BrightGreen => semantic_color::BRIGHT_GREEN,
        NamedColor::BrightYellow => semantic_color::BRIGHT_YELLOW,
        NamedColor::BrightBlue => semantic_color::BRIGHT_BLUE,
        NamedColor::BrightMagenta => semantic_color::BRIGHT_MAGENTA,
        NamedColor::BrightCyan => semantic_color::BRIGHT_CYAN,
        NamedColor::BrightWhite => semantic_color::BRIGHT_WHITE,
        NamedColor::Foreground => semantic_color::FOREGROUND,
        NamedColor::Background => semantic_color::BACKGROUND,
        NamedColor::Cursor => semantic_color::CURSOR,
        _ => semantic_color::FOREGROUND,
    }
}

fn palette_rgb(index: usize) -> Rgb {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    let (r, g, b) = if index < 16 {
        ANSI16[index]
    } else if index < 232 {
        let i = index - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let scale = |v: usize| if v == 0 { 0 } else { 55 + v as u8 * 40 };
        (scale(r), scale(g), scale(b))
    } else if index < 256 {
        let v = 8 + (index - 232) as u8 * 10;
        (v, v, v)
    } else {
        (229, 229, 229)
    };
    Rgb { r, g, b }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: u16, rows: u16) -> TerminalModel {
        TerminalModel::new(cols, rows, 1000, TerminalEventSink::new())
    }

    #[test]
    fn plain_text_and_cursor() {
        let mut t = term(20, 4);
        t.feed(b"hello\r\nworld");
        let text = t.plain_text();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        let snap = t.snapshot();
        assert_eq!(snap.cells.len(), 80);
        assert!(snap.cursor.visible);
        assert_eq!(snap.cursor.x, 5);
        assert_eq!(snap.cursor.y, 1);
    }

    #[test]
    fn ansi_colors_and_bold_are_exported() {
        let mut t = term(20, 3);
        t.feed(b"\x1b[1;31mred\x1b[0m");
        let snap = t.snapshot();
        assert_eq!(snap.cells[0].ch, 'r');
        assert_eq!(snap.cells[0].fg, ColorDto::Named(semantic_color::RED));
        assert_ne!(snap.cells[0].flags & cell_flags::BOLD, 0);
    }

    #[test]
    fn wide_and_combining_characters() {
        let mut t = term(10, 2);
        t.feed("a界e\u{301}".as_bytes());
        let snap = t.snapshot();
        assert_eq!(snap.cells[0].ch, 'a');
        assert_eq!(snap.cells[1].ch, '界');
        assert_ne!(snap.cells[1].flags & cell_flags::WIDE_CHAR, 0);
        assert_ne!(snap.cells[2].flags & cell_flags::WIDE_SPACER, 0);
        assert_eq!(snap.cells[3].ch, 'e');
        assert_eq!(snap.cells[3].zero, vec!['\u{301}']);
    }

    #[test]
    fn alternate_screen_and_modes() {
        let mut t = term(20, 4);
        t.feed(b"\x1b[?1049h\x1b[?25l\x1b[?2004h");
        let snap = t.snapshot();
        assert_ne!(snap.modes & pane_modes::ALT_SCREEN, 0);
        assert_eq!(snap.modes & pane_modes::SHOW_CURSOR, 0);
        assert_ne!(snap.modes & pane_modes::BRACKETED_PASTE, 0);
        assert_eq!(snap.cursor.shape, CursorShape::Hidden);
        t.feed(b"\x1b[?1049l\x1b[?25h");
        assert_eq!(t.snapshot().modes & pane_modes::ALT_SCREEN, 0);
    }

    #[test]
    fn scrollback_and_search() {
        let mut t = term(30, 3);
        for i in 0..50 {
            t.feed(format!("line-{i:03}\r\n").as_bytes());
        }
        assert!(t.history_lines() >= 40);
        t.scroll(10);
        assert_eq!(t.scroll_offset(), 10);
        t.scroll_to_bottom();
        assert_eq!(t.scroll_offset(), 0);
        let (hits, truncated) = t.search("line-042", true, 10);
        assert!(!truncated);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].preview.contains("line-042"));
        let (case_hits, _) = t.search("LINE-042", false, 10);
        assert_eq!(case_hits.len(), 1);
        let (none, _) = t.search("LINE-042", true, 10);
        assert!(none.is_empty());
    }

    #[test]
    fn snapshot_restore_preserves_text_and_marks_cold() {
        let mut source = term(30, 4);
        source.feed(b"alpha\r\nbeta\r\n\x1b[31mred\x1b[0m\r\n");
        let snap = source.recovery_snapshot(64 * 1024, 1234);
        assert!(!snap.ansi.is_empty());
        assert_eq!(snap.sha256.len(), 64);
        let mut target = term(30, 4);
        target.restore_snapshot(&snap.ansi).unwrap();
        assert!(target.cold_restored());
        let text = target.plain_text();
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn snapshot_does_not_replay_blank_bottom_rows() {
        let mut source = term(40, 20);
        source.feed(b"top-marker\r\nsecond-line\r\n");
        let snap = source.recovery_snapshot(64 * 1024, 1234);
        assert!(!snap.ansi.ends_with("\r\n"));
        assert!(snap.ansi.lines().count() <= 2);
        let mut target = term(40, 20);
        target.restore_snapshot(&snap.ansi).unwrap();
        target.feed(b"\r\n[FACT] recovered\r\n");
        let text = target.plain_text();
        assert!(text.contains("top-marker"), "restored text: {text}");
    }

    #[test]
    fn restore_rejects_oversize_and_nul() {
        let mut t = term(10, 2);
        assert!(t.restore_snapshot("has\0nul").is_err());
        let huge = "x".repeat(MAX_RESTORE_BYTES + 1);
        assert!(t.restore_snapshot(&huge).is_err());
    }

    #[test]
    fn resize_is_safe_at_tiny_sizes() {
        let mut t = term(2, 2);
        t.resize(1, 1);
        t.feed(b"x");
        let snap = t.snapshot();
        assert_eq!(snap.cells.len(), 1);
        t.resize(300, 100);
        t.feed(b"y");
        assert_eq!(t.snapshot().cells.len(), 30_000);
    }

    #[test]
    fn malformed_escape_does_not_panic() {
        let mut t = term(10, 3);
        t.feed(b"\x1b[99999999999999999999999;");
        t.feed(b"\x1b]0;title\x07");
        t.feed(b"ok");
        assert!(t.plain_text().contains("ok"));
    }

    #[test]
    fn title_callbacks_are_captured() {
        let mut t = term(10, 2);
        t.feed(b"\x1b]0;my-title\x07");
        assert_eq!(t.title(), "my-title");
    }

    #[test]
    fn mouse_mode_bits() {
        let mut t = term(20, 5);
        t.feed(b"\x1b[?1000h\x1b[?1006h");
        let modes = t.mouse_mode();
        assert_ne!(modes & pane_modes::MOUSE_CLICK, 0);
        assert_ne!(modes & pane_modes::SGR_MOUSE, 0);
    }
}
