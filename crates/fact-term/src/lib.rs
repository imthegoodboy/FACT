//! Terminal emulation wrapper.
//!
//! Alacritty's mature VT parser/model handles ANSI, alternate screen, wide
//! characters and scrollback. This crate adds the bounded frame snapshots and
//! safe event handling used by FACT.

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared state filled by parser callbacks and consumed by the engine.
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
                // the host clipboard from the terminal model.
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
    win32_input: Win32InputScanner,
}

const WIN32_INPUT_ON: &[u8] = b"\x1b[?9001h";
const WIN32_INPUT_OFF: &[u8] = b"\x1b[?9001l";

/// Tracks ConPTY's request for Windows Terminal's win32-input-mode.
///
/// Alacritty does not know private mode 9001 and ignores it, so the raw output
/// is scanned instead. A short tail carries a sequence split across reads.
#[derive(Default)]
struct Win32InputScanner {
    enabled: bool,
    tail: Vec<u8>,
}

impl Win32InputScanner {
    fn scan(&mut self, bytes: &[u8]) {
        let keep = WIN32_INPUT_ON.len() - 1;
        if !self.tail.is_empty() {
            // Only sequences that start in the tail can match here: neither the
            // tail nor the chunk prefix is long enough to hold one alone, so
            // nothing is counted twice.
            let mut joined = self.tail.clone();
            joined.extend_from_slice(&bytes[..bytes.len().min(keep)]);
            self.apply(&joined);
        }
        self.apply(bytes);
        self.tail.extend_from_slice(bytes);
        let excess = self.tail.len().saturating_sub(keep);
        self.tail.drain(..excess);
    }

    fn apply(&mut self, bytes: &[u8]) {
        let mut last = None;
        for (index, window) in bytes.windows(WIN32_INPUT_ON.len()).enumerate() {
            if window[0] != 0x1b {
                continue;
            }
            if window == WIN32_INPUT_ON {
                last = Some((index, true));
            } else if window == WIN32_INPUT_OFF {
                last = Some((index, false));
            }
        }
        if let Some((_, enabled)) = last {
            self.enabled = enabled;
        }
    }
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
            // Agent CLIs (Claude Code, OpenCode, Copilot) query and push kitty
            // keyboard flags to tell Shift+Enter from Enter. The queries pass
            // through ConPTY, so answering them is what makes that work.
            kitty_keyboard: true,
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
            win32_input: Win32InputScanner::default(),
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

    /// Feed bytes from the ConPTY. Parser panics are contained so a malformed
    /// escape sequence can never take down the engine.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.win32_input.scan(bytes);
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
        if self.win32_input.enabled {
            out |= pane_modes::WIN32_INPUT;
        }
        if mode.intersects(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC) {
            out |= pane_modes::KITTY_KEYS;
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
        t.set_scroll_offset(25);
        assert_eq!(t.scroll_offset(), 25);
        t.set_scroll_offset(usize::MAX);
        assert_eq!(t.scroll_offset(), t.history_lines());
        t.set_scroll_offset(0);
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
    fn win32_input_mode_is_tracked_across_reads() {
        let mut model = term(20, 4);
        assert_eq!(model.modes() & pane_modes::WIN32_INPUT, 0);
        model.feed(b"\x1b[?90");
        model.feed(b"01h\x1b[?1004h");
        assert_ne!(model.modes() & pane_modes::WIN32_INPUT, 0);
        model.feed(b"text \x1b[?9001l more");
        assert_eq!(model.modes() & pane_modes::WIN32_INPUT, 0);
        for byte in b"\x1b[?9001h" {
            model.feed(&[*byte]);
        }
        assert_ne!(model.modes() & pane_modes::WIN32_INPUT, 0);
        // The sequence is not rendered as text.
        assert!(!model.plain_text().contains("9001"));
    }

    #[test]
    fn kitty_keyboard_flags_are_reported_and_tracked() {
        let sink = TerminalEventSink::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        sink.attach_writer(tx);
        let mut model = TerminalModel::new(20, 4, 100, sink);
        model.feed(b"\x1b[?u");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[?0u");
        assert_eq!(model.modes() & pane_modes::KITTY_KEYS, 0);
        model.feed(b"\x1b[>5u");
        assert_ne!(model.modes() & pane_modes::KITTY_KEYS, 0);
        model.feed(b"\x1b[?u");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[?5u");
        model.feed(b"\x1b[<u");
        assert_eq!(model.modes() & pane_modes::KITTY_KEYS, 0);
    }

    #[test]
    fn mouse_mode_bits() {
        let mut t = term(20, 5);
        t.feed(b"\x1b[?1000h\x1b[?1006h");
        let modes = t.mouse_mode();
        assert_ne!(modes & pane_modes::MOUSE_CLICK, 0);
        assert_ne!(modes & pane_modes::SGR_MOUSE, 0);
    }

    #[test]
    fn mouse_tracking_modes_are_mutually_exclusive() {
        let mut t = term(20, 5);
        for (sequence, expected) in [
            (b"\x1b[?1000h".as_slice(), pane_modes::MOUSE_CLICK),
            (b"\x1b[?1002h".as_slice(), pane_modes::MOUSE_DRAG),
            (b"\x1b[?1003h".as_slice(), pane_modes::MOUSE_MOTION),
        ] {
            t.feed(sequence);
            assert_eq!(t.mouse_mode(), expected);
        }
    }
}
