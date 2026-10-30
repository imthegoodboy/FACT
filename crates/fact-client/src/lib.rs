//! FACT terminal user interface.
//!
//! The in-process engine owns the shells and terminal state. This crate only
//! renders the frames it receives and forwards keyboard, mouse and clipboard
//! input. It never parses terminal output itself.

mod keymap;
mod render;
mod theme;
mod wallpaper;

pub use wallpaper::{
    find_ffmpeg, image_dimensions, in_windows_terminal, is_video as is_video_file,
    Fit as WallpaperFit, WallpaperSpec, VIDEO_EXTENSIONS,
};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode as CKeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use fact_config::{CompiledKeybinds, Config};
use fact_engine::{EngineClient, EventStream};
use fact_protocol::{
    cell_flags, friendly_title, pane_modes, CellDto, Command, Frame, PaneView, RectDto,
    ResponsePayload, SearchResult, SplitDirection, SplitOrientation, TabView, MAX_PASTE_BYTES,
    MAX_SEARCH_QUERY,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use theme::Theme;

/// How often the reader threads check whether the UI is shutting down. They
/// block in the kernel in between, so this costs nothing while idle.
const READER_POLL: Duration = Duration::from_millis(100);
const STATUS_LIFETIME: Duration = Duration::from_secs(2);
/// Upper bound on one batch of queued events before the screen is redrawn.
const MAX_BATCH: Duration = Duration::from_millis(8);
/// How long to wait for the rest of a paste that arrives as keystrokes.
const PASTE_GAP: Duration = Duration::from_millis(8);
/// Longest stretch of keystrokes gathered into one paste batch.
const MAX_PASTE_BATCH: Duration = Duration::from_millis(250);
/// Keys this close together were not typed by hand (key repeat is ~30 ms).
const PASTE_SPACING: Duration = Duration::from_millis(5);
const DOUBLE_CLICK: Duration = Duration::from_millis(450);

/// Width of the new-tab button in the tab strip.
const NEW_TAB_WIDTH: u16 = 3;
/// Cap a single tab so one long title cannot push every other tab off-screen.
const TAB_MAX_WIDTH: u16 = 24;

pub(crate) const ACTION_INFO: &[(&str, &str)] = &[
    ("split_right", "Split right"),
    ("split_down", "Split down"),
    ("close_pane", "Close pane"),
    ("close_tab", "Close tab / grid"),
    ("restart_pane", "Restart pane"),
    ("zoom", "Toggle maximize / full grid"),
    ("maximize", "Maximize focused pane"),
    ("show_all", "Show all panes"),
    ("minimize", "Minimize pane"),
    ("restore", "Restore minimized panes"),
    ("focus_left", "Focus left"),
    ("focus_right", "Focus right"),
    ("focus_up", "Focus up"),
    ("focus_down", "Focus down"),
    ("next_pane", "Next pane"),
    ("prev_pane", "Previous pane"),
    ("new_tab", "New tab"),
    ("next_tab", "Next tab"),
    ("prev_tab", "Previous tab"),
    ("rename_tab", "Rename tab"),
    ("palette", "Command palette"),
    ("search", "Search pane"),
    ("scroll_mode", "Browse scrollback"),
    ("help", "Help"),
    ("copy", "Copy selection"),
    ("paste", "Paste clipboard"),
    ("quit", "Exit FACT (closes shells)"),
    ("swap_pane", "Swap with next pane"),
    ("move_pane", "Move pane to next tab"),
    ("resize_left", "Resize: move edge left"),
    ("resize_right", "Resize: move edge right"),
    ("resize_up", "Resize: move edge up"),
    ("resize_down", "Resize: move edge down"),
    ("balance", "Balance pane sizes"),
    ("cycle_theme", "Cycle color theme"),
];

pub(crate) fn action_label(action: &str) -> &'static str {
    ACTION_INFO
        .iter()
        .find(|(id, _)| *id == action)
        .map(|(_, label)| *label)
        .unwrap_or("Unknown action")
}

/// Run the interactive terminal UI. Terminal state is always restored before
/// returning, including on errors. `wallpaper` is painted behind the panes.
pub fn run(
    client: EngineClient,
    events: EventStream,
    config: Config,
    wallpaper: Option<WallpaperSpec>,
) -> Result<()> {
    let keys = config
        .compiled_keybinds()
        .context("could not compile keyboard bindings")?;
    // FACT is a terminal: it shows the colors its programs produce. NO_COLOR
    // is for programs that emit color, and the panes still inherit it, so
    // crossterm must not strip every color from FACT's own drawing.
    crossterm::style::Colored::set_ansi_color_disabled(false);
    let mut terminal = TerminalGuard::enter(config.mouse)?;
    let size = terminal.size().context("could not read terminal size")?;
    let events = spawn_readers(events);

    let mut app = App::new(client, config, keys, events, size.width, size.height);
    if let Some(spec) = wallpaper {
        match wallpaper::Wallpaper::load(&spec) {
            Ok(loaded) => {
                app.theme = app.theme.with_transparency(true);
                app.wallpaper = Some(RefCell::new(loaded));
                app.sync_wallpaper_base();
            }
            Err(error) => app.set_status(format!("Wallpaper: {error}")),
        }
    }
    let result = app.event_loop(&mut terminal);
    app.stop_events();
    let message = app.exit_message.clone();
    terminal.restore();

    match result {
        Ok(()) => {
            if let Some(message) = message {
                println!("{message}");
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

struct UiEvents {
    receiver: Receiver<UiEvent>,
    cancel: Arc<AtomicBool>,
}

/// Merge engine frames and terminal input into one queue so the UI thread can
/// sleep until something actually happens, then redraw once.
fn spawn_readers(mut stream: EventStream) -> UiEvents {
    // Bounded: a busy agent cannot grow an unbounded queue while drawing or
    // clipboard work falls behind. The engine drops and resends frames instead.
    let (tx, rx) = mpsc::sync_channel(64);
    let cancel = Arc::new(AtomicBool::new(false));

    let frames_tx = tx.clone();
    let frames_cancel = Arc::clone(&cancel);
    thread::Builder::new()
        .name("fact-ui-frames".into())
        .spawn(move || {
            while !frames_cancel.load(Ordering::Acquire) {
                match stream.recv_timeout(READER_POLL) {
                    Ok(None) => {}
                    Ok(Some(message)) => {
                        if frames_tx.send(UiEvent::Message(Box::new(message))).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = frames_tx.send(UiEvent::Closed(error.to_string()));
                        break;
                    }
                }
            }
        })
        .expect("spawning the frame reader thread cannot fail");

    let input_cancel = Arc::clone(&cancel);
    thread::Builder::new()
        .name("fact-ui-input".into())
        .spawn(move || {
            while !input_cancel.load(Ordering::Acquire) {
                let event = match event::poll(READER_POLL) {
                    Ok(false) => continue,
                    Ok(true) => event::read(),
                    Err(error) => Err(error),
                };
                // Keys carry when they arrived: a paste arrives as a burst
                // of keys microseconds apart, typing never does.
                let message = match event {
                    Ok(Event::Key(key)) => UiEvent::Key(key, Instant::now()),
                    Ok(event) => UiEvent::Input(event),
                    Err(error) => UiEvent::InputFailed(error.to_string()),
                };
                let failed = matches!(message, UiEvent::InputFailed(_));
                if tx.send(message).is_err() || failed {
                    break;
                }
            }
        })
        .expect("spawning the input reader thread cannot fail");

    UiEvents {
        receiver: rx,
        cancel,
    }
}

enum UiEvent {
    Message(Box<fact_protocol::ServerMessage>),
    Closed(String),
    Key(KeyEvent, Instant),
    Input(Event),
    InputFailed(String),
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    mouse: bool,
    active: bool,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("could not enable terminal raw mode")?;
        let mut stdout = io::stdout();
        let setup = (|| -> io::Result<()> {
            execute!(
                stdout,
                EnterAlternateScreen,
                Hide,
                EnableBracketedPaste,
                EnableFocusChange
            )?;
            if mouse {
                execute!(stdout, EnableMouseCapture)?;
            }
            Ok(())
        })();
        if let Err(error) = setup {
            leave_terminal(mouse);
            return Err(error).context("could not enter the alternate screen");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                leave_terminal(mouse);
                return Err(error).context("could not initialize the terminal renderer");
            }
        };
        Ok(Self {
            terminal,
            mouse,
            active: true,
        })
    }

    fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.terminal.size()
    }

    fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        leave_terminal(self.mouse);
        let _ = self.terminal.show_cursor();
    }
}

fn leave_terminal(mouse: bool) {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange);
    if mouse {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    let _ = execute!(stdout, Show, LeaveAlternateScreen);
    let _ = stdout.flush();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Debug, Clone)]
struct PaneBuffer {
    cols: u16,
    rows: u16,
    cells: Vec<CellDto>,
}

impl PaneBuffer {
    fn cell(&self, x: u16, y: u16) -> Option<&CellDto> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y as usize * self.cols as usize + x as usize)
    }

    /// Bounds of the word under the cursor. A double-click uses this to select
    /// the same way Windows Terminal and PowerShell do.
    fn word_bounds(&self, x: u16, y: u16) -> (u16, u16) {
        let Some(cell) = self.cell(x, y) else {
            return (x, x);
        };
        if !is_word_cell(cell) {
            return (x, x);
        }
        let mut start = x;
        while start > 0 && self.cell(start - 1, y).is_some_and(is_word_cell) {
            start -= 1;
        }
        let mut end = x;
        while end + 1 < self.cols && self.cell(end + 1, y).is_some_and(is_word_cell) {
            end += 1;
        }
        (start, end)
    }

    /// Bounds of a full visual line for a triple-click.
    fn line_bounds(&self) -> (u16, u16) {
        (0, self.cols.saturating_sub(1))
    }
}

fn is_word_cell(cell: &CellDto) -> bool {
    cell.flags & cell_flags::WIDE_SPACER == 0
        && !cell.ch.is_whitespace()
        && !matches!(
            cell.ch,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '"'
                | '\''
                | '`'
                | ','
                | ';'
                | '|'
                | '&'
        )
}

#[derive(Debug, Clone)]
struct Selection {
    pane: u64,
    anchor: (u16, u16),
    end: (u16, u16),
    /// Keep the selected viewport stable while the child continues streaming.
    buffer: Option<PaneBuffer>,
}

/// Each button keeps its own originating pane during overlapping gestures.
#[derive(Default)]
struct AppMouseCapture {
    buttons: [Option<u64>; 3],
}

impl AppMouseCapture {
    fn index(button: MouseButton) -> usize {
        match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }

    fn press(&mut self, button: MouseButton, pane: u64) {
        self.buttons[Self::index(button)] = Some(pane);
    }

    fn pane(&self, button: MouseButton) -> Option<u64> {
        self.buttons[Self::index(button)]
    }

    fn release(&mut self, button: MouseButton) -> Option<u64> {
        self.buttons[Self::index(button)].take()
    }
}

fn selection_text(buffers: &HashMap<u64, PaneBuffer>, selection: &Selection) -> Option<String> {
    let buffer = selection
        .buffer
        .as_ref()
        .or_else(|| buffers.get(&selection.pane))?;
    if buffer.cols == 0 || buffer.rows == 0 {
        return None;
    }
    let (start, end) = selection.normalized();
    let start = (start.0.min(buffer.cols - 1), start.1.min(buffer.rows - 1));
    let end = (end.0.min(buffer.cols - 1), end.1.min(buffer.rows - 1));
    let mut output = String::new();
    for y in start.1..=end.1 {
        let first = if y == start.1 { start.0 } else { 0 };
        let last = if y == end.1 { end.0 } else { buffer.cols - 1 };
        let mut line = String::new();
        for x in first..=last {
            let Some(cell) = buffer.cell(x, y) else {
                continue;
            };
            if cell.flags & cell_flags::WIDE_SPACER != 0 {
                continue;
            }
            if cell.flags & cell_flags::HIDDEN != 0 {
                line.push(' ');
            } else {
                line.push_str(&cell.symbol());
            }
        }
        output.push_str(line.trim_end());
        if y != end.1 {
            output.push('\n');
        }
    }
    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

#[derive(Debug, Clone, Copy)]
struct ClickTracker {
    at: Instant,
    pane: u64,
    x: u16,
    y: u16,
    count: u8,
}

impl Selection {
    fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor.1, self.anchor.0) <= (self.end.1, self.end.0) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        let (start, end) = self.normalized();
        if y < start.1 || y > end.1 {
            return false;
        }
        if start.1 == end.1 {
            return x >= start.0 && x <= end.0;
        }
        if y == start.1 {
            return x >= start.0;
        }
        if y == end.1 {
            return x <= end.0;
        }
        true
    }
}

#[derive(Debug, Clone)]
struct DragSplit {
    path: Vec<u8>,
    orientation: SplitOrientation,
    region: RectDto,
    last_sent: Instant,
}

/// Where an Alt+dragged pane would land if released now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneDropTarget {
    Pane(u64),
    Tab(u32),
}

/// Alt+drag moves a pane: drop it on another pane to swap, or on a tab.
#[derive(Debug, Clone, Copy)]
struct PaneDrag {
    pane: u64,
    target: Option<PaneDropTarget>,
}

struct TabRange {
    id: u32,
    label: String,
    rect: RectDto,
}

fn pack_tab_ranges(tabs: &[TabView], width: u16, start: usize) -> Vec<TabRange> {
    let limit = width.saturating_sub(NEW_TAB_WIDTH + 1);
    let mut x = 0u16;
    let mut ranges = Vec::new();
    for tab in &tabs[start.min(tabs.len())..] {
        if x >= limit {
            break;
        }
        let available = limit.saturating_sub(x).min(TAB_MAX_WIDTH);
        if available < 4 {
            break;
        }
        let raw = format!(" {} ", tab.title);
        let label = fit_text(&raw, usize::from(available));
        let rect = RectDto {
            x,
            y: 0,
            width: label.chars().count() as u16,
            height: 1,
        };
        if rect.width == 0 {
            break;
        }
        ranges.push(TabRange {
            id: tab.id,
            label,
            rect,
        });
        x = rect.x.saturating_add(rect.width);
        if x < limit {
            x = x.saturating_add(1);
        }
    }
    ranges
}

fn tab_ranges(tabs: &[TabView], width: u16) -> Vec<TabRange> {
    let ranges = pack_tab_ranges(tabs, width, 0);
    if tabs.is_empty()
        || ranges
            .iter()
            .any(|range| tabs.iter().any(|tab| tab.id == range.id && tab.active))
    {
        return ranges;
    }
    let Some(active) = tabs.iter().position(|tab| tab.active) else {
        return ranges;
    };
    // Keep the active tab clickable when the strip overflows. Starting at the
    // first index whose window includes it preserves as many preceding tabs as
    // possible while still showing the tab the user is working in.
    for start in 1..=active {
        let window = pack_tab_ranges(tabs, width, start);
        if window
            .iter()
            .any(|range| tabs.iter().any(|tab| tab.id == range.id && tab.active))
        {
            return window;
        }
    }
    ranges
}

fn new_tab_rect(width: u16) -> Option<RectDto> {
    (width >= NEW_TAB_WIDTH).then(|| RectDto {
        x: width - NEW_TAB_WIDTH,
        y: 0,
        width: NEW_TAB_WIDTH,
        height: 1,
    })
}

fn minimized_tray_rect(width: u16, height: u16, count: usize) -> Option<RectDto> {
    if count == 0 || width < 8 || height == 0 {
        return None;
    }
    let label = format!("▾ {count} ");
    let tray_width = label.chars().count() as u16;
    Some(RectDto {
        x: width.saturating_sub(tray_width),
        y: height.saturating_sub(1),
        width: tray_width,
        height: 1,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollMode {
    /// FACT owns the scrollback and the thumb reflects the current offset.
    History,
    /// A full-screen application owns its viewport and receives scroll gestures.
    App,
}

/// How scrolling should be routed for a pane.
///
/// Full-screen applications such as Claude Code, Codex and OpenCode use the
/// alternate screen by default. That screen has no FACT history, so pretending
/// there is scrollback would be misleading. Instead, expose an app-scroll mode
/// that forwards the standard terminal scroll gestures to the application.
fn pane_scroll_mode(pane: &PaneView) -> Option<ScrollMode> {
    if pane.modes & pane_modes::ALT_SCREEN != 0 {
        Some(ScrollMode::App)
    } else if pane.history_lines > 0 {
        Some(ScrollMode::History)
    } else {
        None
    }
}

fn app_tracks_mouse(modes: u32) -> bool {
    modes & (pane_modes::MOUSE_CLICK | pane_modes::MOUSE_DRAG | pane_modes::MOUSE_MOTION) != 0
}

#[derive(Debug, Clone)]
struct ScrollbarGeometry {
    pane: u64,
    track: RectDto,
    thumb_y: u16,
    thumb_height: u16,
    max_offset: usize,
    scroll_offset: usize,
}

fn scrollbar_geometry(pane: &PaneView) -> Option<ScrollbarGeometry> {
    let track_height = pane.slot.height.min(pane.content_rows);
    if track_height == 0 || pane.slot.width < 2 {
        return None;
    }
    // App-owned viewports keep their own UI and receive the entire terminal
    // width. FACT only overlays a history scrollbar in ordinary shell panes.
    if pane.modes & pane_modes::ALT_SCREEN != 0
        || app_tracks_mouse(pane.modes)
        || pane.history_lines == 0
    {
        return None;
    }
    let history = pane.history_lines;
    let total = history.saturating_add(usize::from(pane.content_rows));
    let minimum = 2_u16.min(track_height);
    let maximum = (track_height / 2).max(minimum);
    let proportional = (usize::from(track_height) * usize::from(track_height) / total) as u16;
    let thumb_height = proportional.clamp(minimum, maximum);
    let span = track_height.saturating_sub(thumb_height);
    let scroll_offset = pane.scroll_offset.min(history);
    let thumb_top =
        span.saturating_sub(((u64::from(span) * scroll_offset as u64) / history as u64) as u16);
    Some(ScrollbarGeometry {
        pane: pane.id,
        track: RectDto {
            x: pane
                .slot
                .x
                .saturating_add(pane.slot.width)
                .saturating_sub(1),
            y: pane.slot.y,
            width: 1,
            height: track_height,
        },
        thumb_y: pane.slot.y.saturating_add(thumb_top),
        thumb_height,
        max_offset: history,
        scroll_offset,
    })
}

fn scrollbar_hovered(
    geometry: &ScrollbarGeometry,
    position: Option<(u16, u16)>,
    tabs_height: u16,
) -> bool {
    position.is_some_and(|(x, y)| {
        y.checked_sub(tabs_height).is_some_and(|y| {
            x >= geometry.track.x.saturating_sub(1)
                && geometry.track.contains(x.max(geometry.track.x), y)
        })
    })
}

fn scrollbar_thumb_span(geometry: &ScrollbarGeometry) -> u16 {
    geometry.track.height.saturating_sub(geometry.thumb_height)
}

fn scrollbar_jump_offset(geometry: &ScrollbarGeometry, content_y: u16) -> usize {
    let span = scrollbar_thumb_span(geometry);
    if geometry.max_offset == 0 || span == 0 {
        return 0;
    }
    let relative = content_y
        .saturating_sub(geometry.track.y)
        .min(geometry.track.height.saturating_sub(1));
    let thumb_top = relative.saturating_sub(geometry.thumb_height / 2).min(span);
    let numerator = u64::from(span.saturating_sub(thumb_top)) * geometry.max_offset as u64
        + u64::from(span) / 2;
    (numerator / u64::from(span)) as usize
}

fn scrollbar_drag_offset(
    geometry: &ScrollbarGeometry,
    anchor_y: u16,
    base_offset: usize,
    content_y: u16,
) -> usize {
    let span = scrollbar_thumb_span(geometry);
    if geometry.max_offset == 0 || span == 0 {
        return base_offset.min(geometry.max_offset);
    }
    let moved = (f64::from(i32::from(content_y) - i32::from(anchor_y)) / f64::from(span)
        * geometry.max_offset as f64)
        .round() as i64;
    (base_offset as i64 - moved).clamp(0, geometry.max_offset as i64) as usize
}

#[derive(Debug, Clone)]
struct ScrollDrag {
    geometry: ScrollbarGeometry,
    last_sent: Instant,
    anchor_y: u16,
    base_offset: usize,
}

#[derive(Debug)]
enum Overlay {
    Help(HelpState),
    Palette(PaletteState),
    Search(SearchState),
    Prompt(PromptState),
    Confirm(ConfirmState),
    Context(ContextState),
}

#[derive(Debug, Default)]
struct HelpState {
    scroll: usize,
}

#[derive(Debug)]
struct PaletteState {
    query: String,
    selected: usize,
}

#[derive(Debug)]
struct SearchState {
    query: String,
    result: Option<SearchResult>,
    selected: usize,
    case_sensitive: bool,
}

#[derive(Debug)]
struct PromptState {
    title: String,
    value: String,
    kind: PromptKind,
}

#[derive(Debug)]
enum PromptKind {
    RenameTab { tab: u32 },
}

#[derive(Debug)]
struct ConfirmState {
    message: String,
    command: Command,
}

#[derive(Debug)]
struct ContextState {
    column: u16,
    row: u16,
    selected: usize,
    actions: Vec<&'static str>,
}

impl ContextState {
    // Share clipping and list scrolling between rendering and mouse hit testing.
    fn layout(
        &self,
        area: ratatui::layout::Rect,
        keys: &CompiledKeybinds,
    ) -> Option<(ratatui::layout::Rect, usize)> {
        let desired_width = self
            .actions
            .iter()
            .map(|action| {
                action_label(action).chars().count().max(16)
                    + keys.describe(action).chars().count()
                    + 4
            })
            .max()
            .unwrap_or(8);
        let width = desired_width.min(usize::from(area.width.saturating_sub(2))) as u16;
        let height = (self.actions.len() as u16 + 2).min(area.height.saturating_sub(2));
        if width < 8 || height < 3 {
            return None;
        }
        let x = self
            .column
            .min(area.right().saturating_sub(width))
            .max(area.x);
        let y = self
            .row
            .min(area.bottom().saturating_sub(height))
            .max(area.y);
        let visible = usize::from(height - 2);
        let start = self
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(self.actions.len().saturating_sub(1));
        Some((ratatui::layout::Rect::new(x, y, width, height), start))
    }
}

pub(crate) struct PaletteItem {
    pub action: &'static str,
    pub label: &'static str,
    pub shortcut: String,
}

struct App {
    client: EngineClient,
    config: Config,
    keys: CompiledKeybinds,
    theme: Theme,
    events: UiEvents,
    frame: Option<Frame>,
    buffers: HashMap<u64, PaneBuffer>,
    last_seq: u64,
    last_bells: HashMap<u64, u64>,
    bells: HashSet<u64>,
    width: u16,
    height: u16,
    overlay: Option<Overlay>,
    prefix_armed: bool,
    selection: Option<Selection>,
    last_click: Option<ClickTracker>,
    drag_split: Option<DragSplit>,
    /// Previous gutter press, for double-click-to-balance.
    last_gutter_click: Option<(Vec<u8>, Instant)>,
    scroll_drag: Option<ScrollDrag>,
    /// Tab being dragged along the tab strip.
    tab_drag: Option<u32>,
    pane_drag: Option<PaneDrag>,
    mouse_position: Option<(u16, u16)>,
    scroll_mode: Option<u64>,
    app_mouse_down: AppMouseCapture,
    /// Characters whose key is down, to tell a key's release from a console
    /// alt code, which arrives as a release alone.
    held_chars: HashSet<char>,
    /// The window was resized during this batch; tell the engine once.
    resize_pending: bool,
    status: Option<(String, Instant)>,
    exit_message: Option<String>,
    fatal: Option<anyhow::Error>,
    /// The screen no longer matches the state; draw before sleeping again.
    dirty: bool,
    /// Painted behind the panes; `RefCell` because drawing advances its cache.
    wallpaper: Option<RefCell<wallpaper::Wallpaper>>,
}

impl App {
    fn new(
        client: EngineClient,
        config: Config,
        keys: CompiledKeybinds,
        events: UiEvents,
        width: u16,
        height: u16,
    ) -> Self {
        let theme =
            Theme::by_name(&config.theme).with_transparency(config.transparent_background());
        Self {
            client,
            config,
            keys,
            theme,
            events,
            frame: None,
            buffers: HashMap::new(),
            last_seq: 0,
            last_bells: HashMap::new(),
            bells: HashSet::new(),
            width,
            height,
            overlay: None,
            prefix_armed: false,
            selection: None,
            last_click: None,
            drag_split: None,
            last_gutter_click: None,
            scroll_drag: None,
            tab_drag: None,
            pane_drag: None,
            mouse_position: None,
            scroll_mode: None,
            app_mouse_down: AppMouseCapture::default(),
            held_chars: HashSet::new(),
            resize_pending: false,
            status: None,
            exit_message: None,
            fatal: None,
            dirty: true,
            wallpaper: None,
        }
    }

    /// Sleep until input or a frame arrives, apply everything queued, then
    /// draw once. An idle workspace does no rendering work at all.
    fn event_loop(&mut self, terminal: &mut TerminalGuard) -> Result<()> {
        self.resize_session();
        while self.fatal.is_none() && self.exit_message.is_none() {
            if self.dirty {
                self.dirty = false;
                terminal
                    .terminal()
                    .draw(|frame| self.render(frame))
                    .context("could not draw the FACT UI")?;
            }
            let status_wait = self
                .status
                .as_ref()
                .map(|(_, at)| STATUS_LIFETIME.saturating_sub(at.elapsed()))
                .unwrap_or(Duration::from_secs(60));
            let frame_wait = self
                .wallpaper
                .as_ref()
                .and_then(|wallpaper| wallpaper.borrow().next_frame_at())
                .map(|at| at.saturating_duration_since(Instant::now()));
            let wait = frame_wait.map_or(status_wait, |frame| frame.min(status_wait));
            let mut batch = Vec::new();
            match self.events.receiver.recv_timeout(wait) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.fatal = Some(anyhow!("FACT event readers stopped unexpectedly"));
                }
            }
            let started = Instant::now();
            while self.fatal.is_none() {
                let pasting = trailing_text_keys(&batch) >= 3;
                let limit = if pasting { MAX_PASTE_BATCH } else { MAX_BATCH };
                if started.elapsed() >= limit {
                    break;
                }
                match self.events.receiver.try_recv() {
                    Ok(event) => batch.push(event),
                    // Text pasted into the host terminal arrives as a stream
                    // of keystrokes. Wait briefly for the rest so the whole
                    // paste is handled at once.
                    Err(TryRecvError::Empty) if pasting => {
                        match self.events.receiver.recv_timeout(PASTE_GAP) {
                            Ok(event) => batch.push(event),
                            Err(_) => break,
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.fatal = Some(anyhow!("FACT event readers stopped unexpectedly"));
                    }
                }
            }
            self.handle_batch(batch);
            if std::mem::take(&mut self.resize_pending) {
                self.resize_session();
            }
            if self
                .status
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= STATUS_LIFETIME)
            {
                self.status = None;
                self.dirty = true;
            }
            let message = self.wallpaper.as_ref().and_then(|wallpaper| {
                let mut wallpaper = wallpaper.borrow_mut();
                if wallpaper.tick(Instant::now()) {
                    self.dirty = true;
                }
                wallpaper.take_message()
            });
            if let Some(message) = message {
                self.set_status(message);
                self.dirty = true;
            }
        }

        if let Some(error) = self.fatal.take() {
            return Err(error);
        }
        Ok(())
    }

    /// Apply a batch of events in order. Windows Terminal types pasted text
    /// into console programs as keystrokes; a burst of them (keys a few
    /// microseconds apart) holding a line break followed by more text goes to
    /// the pane as one paste instead of running line by line.
    fn handle_batch(&mut self, batch: Vec<UiEvent>) {
        let mut run: Vec<KeyEvent> = Vec::new();
        let mut last_key_at: Option<Instant> = None;
        for event in batch {
            if self.fatal.is_some() || self.exit_message.is_some() {
                break;
            }
            let (key, at) = match event {
                UiEvent::Key(key, at) => (key, at),
                // Frames only change what is drawn; a paste stays whole.
                UiEvent::Message(message) => {
                    self.handle_ui_event(UiEvent::Message(message));
                    continue;
                }
                other => {
                    self.flush_keys(&mut run);
                    self.handle_ui_event(other);
                    continue;
                }
            };
            let Some(key) = key_press(&mut self.held_chars, key) else {
                continue;
            };
            // A pause between keys ends a burst: what came before was typed.
            if last_key_at.is_some_and(|last| at.saturating_duration_since(last) > PASTE_SPACING) {
                self.flush_keys(&mut run);
            }
            last_key_at = Some(at);
            if is_text_key(&key) {
                run.push(key);
            } else {
                self.flush_keys(&mut run);
                self.handle_key(key);
                self.dirty = true;
            }
        }
        self.flush_keys(&mut run);
    }

    fn flush_keys(&mut self, run: &mut Vec<KeyEvent>) {
        let keys = std::mem::take(run);
        if keys.is_empty() {
            return;
        }
        self.dirty = true;
        let plain = self.overlay.is_none() && self.scroll_mode.is_none() && !self.prefix_armed;
        match pasted_text(&keys).filter(|_| plain) {
            Some(text) => self.paste_text(text),
            None => {
                for key in keys {
                    self.handle_key(key);
                }
            }
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Message(message) => {
                self.handle_message(*message);
                self.dirty = true;
            }
            UiEvent::Closed(reason) => {
                self.fatal = Some(anyhow!("FACT engine event stream closed: {reason}"));
            }
            UiEvent::InputFailed(reason) => {
                self.fatal = Some(anyhow!("could not read terminal input: {reason}"));
            }
            UiEvent::Key(key, _) => {
                if let Some(key) = key_press(&mut self.held_chars, key) {
                    self.handle_key(key);
                    self.dirty = true;
                }
            }
            UiEvent::Input(event) => self.handle_input(event),
        }
    }

    fn handle_input(&mut self, event: Event) {
        match event {
            Event::Mouse(mouse) if self.config.mouse => {
                // Pointer motion only matters to the screen while it changes
                // a hover state or a drag; skip redraws for idle movement.
                let before = self.hover_state();
                let motion = matches!(mouse.kind, MouseEventKind::Moved);
                self.handle_mouse(mouse);
                if !motion || before != self.hover_state() {
                    self.dirty = true;
                }
            }
            Event::Paste(text) => {
                self.paste_text(text);
                self.dirty = true;
            }
            Event::FocusGained => {
                self.held_chars.clear();
                if let Some(wallpaper) = &self.wallpaper {
                    wallpaper.borrow_mut().set_paused(false);
                }
            }
            Event::FocusLost => {
                self.held_chars.clear();
                // An animated wallpaper costs nothing while nobody watches.
                if let Some(wallpaper) = &self.wallpaper {
                    wallpaper.borrow_mut().set_paused(true);
                }
                self.mouse_position = None;
                self.scroll_drag = None;
                self.drag_split = None;
                self.tab_drag = None;
                self.pane_drag = None;
                self.dirty = true;
            }
            Event::Resize(cols, rows) => {
                // Dragging a window edge sends many sizes; the engine only
                // needs the last one of each batch.
                self.selection = None;
                self.width = cols;
                self.height = rows;
                self.resize_pending = true;
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Let the wallpaper fade toward the current theme's background.
    fn sync_wallpaper_base(&self) {
        if let (Some(wallpaper), ratatui::style::Color::Rgb(r, g, b)) =
            (&self.wallpaper, self.theme.bg)
        {
            wallpaper.borrow_mut().set_base([r, g, b]);
        }
    }

    /// Pane whose hover-only scrollbar is currently shown, if any.
    fn hover_state(&self) -> Option<u64> {
        self.frame
            .as_ref()?
            .panes
            .iter()
            .find_map(|pane| self.visible_scrollbar(pane).map(|geometry| geometry.pane))
    }

    fn handle_message(&mut self, message: fact_protocol::ServerMessage) {
        match message {
            fact_protocol::ServerMessage::Frame(frame) => self.handle_frame(*frame),
            fact_protocol::ServerMessage::SessionEnded { reason } => {
                self.exit_message = Some(format!("FACT stopped: {reason}"));
            }
        }
    }

    fn handle_frame(&mut self, mut frame: Frame) {
        if frame.seq <= self.last_seq {
            return;
        }
        if self.selection.as_ref().is_some_and(|selection| {
            !frame.panes.iter().any(|pane| {
                pane.id == selection.pane
                    && selection.buffer.as_ref().is_none_or(|buffer| {
                        buffer.cols == pane.content_cols && buffer.rows == pane.content_rows
                    })
            })
        }) {
            self.selection = None;
        }
        for pane in &mut frame.panes {
            // Move the cells into the pane buffer; rendering only reads buffers,
            // so the frame does not need to keep a second copy.
            if let Some(cells) = pane.cells.take() {
                let expected = usize::from(pane.content_cols) * usize::from(pane.content_rows);
                if expected == cells.len() {
                    self.buffers.insert(
                        pane.id,
                        PaneBuffer {
                            cols: pane.content_cols,
                            rows: pane.content_rows,
                            cells,
                        },
                    );
                }
            }
            let previous = self.last_bells.insert(pane.id, pane.bell_seq).unwrap_or(0);
            if pane.bell_seq > previous && self.last_seq != 0 && !pane.focused {
                self.bells.insert(pane.id);
            }
        }

        // Panes that left the screen send their cells again when they return.
        self.buffers
            .retain(|id, _| frame.panes.iter().any(|pane| pane.id == *id));
        self.last_seq = frame.seq;
        self.frame = Some(frame);
    }

    /// Focus `pane` and treat it as focused right away. The frame confirming
    /// it comes later; keys and pastes meanwhile must already reach it.
    fn focus(&mut self, pane: u64) {
        self.bells.remove(&pane);
        if self.send(Command::FocusPane { pane }).is_none() {
            return;
        }
        if let Some(frame) = &mut self.frame {
            if frame.panes.iter().any(|view| view.id == pane) {
                frame.focus = Some(pane);
                for view in &mut frame.panes {
                    view.focused = view.id == pane;
                }
            }
        }
        // Scroll mode belongs to the pane it started in.
        if self.scroll_mode.is_some_and(|scrolled| scrolled != pane) {
            self.scroll_mode = None;
            self.set_status("Scroll mode ended.");
        }
    }

    fn focused_pane(&self) -> Option<&PaneView> {
        let frame = self.frame.as_ref()?;
        if let Some(id) = frame.focus {
            if let Some(pane) = frame.panes.iter().find(|pane| pane.id == id) {
                return Some(pane);
            }
        }
        frame.panes.first()
    }

    fn focused_id(&self) -> Option<u64> {
        self.focused_pane().map(|pane| pane.id)
    }

    fn enter_scroll_mode(&mut self) {
        let Some(pane) = self.focused_pane() else {
            self.set_status("No pane to scroll.");
            return;
        };
        let pane_id = pane.id;
        let history = pane.history_lines;
        let mode = pane_scroll_mode(pane);
        match mode {
            Some(ScrollMode::History) => {
                self.selection = None;
                self.scroll_mode = Some(pane_id);
                let lines = if history == 1 { "line" } else { "lines" };
                self.set_status(format!(
                    "Scroll mode: {history} {lines}. Up/Down, PgUp/PgDn, Home/End, Esc exits."
                ));
            }
            Some(ScrollMode::App) => {
                self.selection = None;
                self.scroll_mode = Some(pane_id);
                self.set_status(
                    "App scroll: the full-screen app owns its history. PgUp/PgDn, Home/End, wheel; Esc exits.",
                );
            }
            None => self.set_status("No scrollback above this pane yet."),
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        let Some(pane_id) = self.scroll_mode else {
            return;
        };
        let Some((mode, app_cursor, history, rows)) = self
            .frame
            .as_ref()
            .and_then(|frame| frame.panes.iter().find(|pane| pane.id == pane_id))
            .map(|pane| {
                (
                    pane_scroll_mode(pane),
                    pane.modes & pane_modes::APP_CURSOR != 0,
                    pane.history_lines,
                    pane.content_rows,
                )
            })
        else {
            self.scroll_mode = None;
            self.set_status("The scrolled pane closed; scroll mode ended.");
            return;
        };
        match mode {
            Some(ScrollMode::App) => self.handle_app_scroll_key(pane_id, key, app_cursor),
            Some(ScrollMode::History) => {
                let page = i64::from(rows.max(1));
                match key.code {
                    CKeyCode::Esc => {
                        self.scroll_mode = None;
                        self.set_status("Scroll mode closed.");
                    }
                    CKeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => {
                        self.scroll_mode = None;
                        self.set_status("Scroll mode closed.");
                    }
                    CKeyCode::Up => self.scroll_relative(pane_id, 1),
                    CKeyCode::Down => self.scroll_relative(pane_id, -1),
                    CKeyCode::PageUp => self.scroll_relative(pane_id, page),
                    CKeyCode::PageDown => self.scroll_relative(pane_id, -page),
                    CKeyCode::Home | CKeyCode::Char('g') => {
                        self.scroll_to(pane_id, history, history);
                    }
                    CKeyCode::End | CKeyCode::Char('G') => {
                        self.scroll_to(pane_id, 0, history);
                    }
                    _ => self
                        .set_status("Scroll mode: Up/Down, PgUp/PgDn, Home/End (g/G), Esc exits."),
                }
            }
            None => {
                self.scroll_mode = None;
                self.set_status("Scroll mode closed; this pane no longer has scrollback.");
            }
        }
    }

    /// Forward a standard terminal scroll key to a full-screen application.
    /// The application owns both its viewport and its scrollbar.
    fn handle_app_scroll_key(&mut self, pane: u64, key: KeyEvent, app_cursor: bool) {
        match key.code {
            CKeyCode::Esc => {
                self.scroll_mode = None;
                self.set_status("App scroll closed.");
            }
            CKeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => {
                self.scroll_mode = None;
                self.set_status("App scroll closed.");
            }
            CKeyCode::Up
            | CKeyCode::Down
            | CKeyCode::PageUp
            | CKeyCode::PageDown
            | CKeyCode::Home
            | CKeyCode::End => {
                if let Some(bytes) =
                    keymap::encode_key(KeyEvent::new(key.code, key.modifiers), app_cursor)
                {
                    let _ = self.send(Command::Input { pane, data: bytes });
                }
            }
            _ => self.set_status("App scroll: PgUp/PgDn, Up/Down, Home/End or wheel; Esc exits."),
        }
    }

    fn send_app_scroll(&mut self, pane: u64, up: bool, page: bool) {
        let app_cursor = self
            .frame
            .as_ref()
            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
            .is_some_and(|view| view.modes & pane_modes::APP_CURSOR != 0);
        let code = if page {
            if up {
                CKeyCode::PageUp
            } else {
                CKeyCode::PageDown
            }
        } else if up {
            CKeyCode::Up
        } else {
            CKeyCode::Down
        };
        if let Some(bytes) = keymap::encode_key(KeyEvent::new(code, KeyModifiers::NONE), app_cursor)
        {
            let _ = self.send(Command::Input { pane, data: bytes });
        }
    }

    /// Scroll mode synthesizes a wheel at the centre for mouse-aware apps;
    /// keyboard-only apps receive PageUp/PageDown to browse their transcript.
    fn send_app_wheel(&mut self, pane: u64, up: bool) {
        let geometry = self
            .frame
            .as_ref()
            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
            .map(|view| (view.modes, view.content_cols / 2, view.content_rows / 2));
        match geometry {
            Some((modes, x, y)) if app_tracks_mouse(modes) => {
                let kind = if up {
                    MouseEventKind::ScrollUp
                } else {
                    MouseEventKind::ScrollDown
                };
                if let Some(data) = keymap::encode_mouse(modes, kind, x, y, KeyModifiers::NONE) {
                    let _ = self.send(Command::Input { pane, data });
                } else {
                    self.send_app_scroll(pane, up, true);
                }
            }
            _ => self.send_app_scroll(pane, up, true),
        }
    }

    fn scroll_relative(&mut self, pane: u64, delta: i64) {
        self.selection = None;
        let delta = delta.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let _ = self.send(Command::ScrollPane { pane, delta });
    }

    fn scroll_to(&mut self, pane: u64, offset: usize, history: usize) {
        self.selection = None;
        let _ = self.send(Command::SetScrollOffset { pane, offset });
        self.set_status(format!("Scroll {offset}/{history} — Esc exits."));
    }

    fn active_tab(&self) -> Option<&TabView> {
        self.frame.as_ref()?.tabs.iter().find(|tab| tab.active)
    }

    fn active_tab_id(&self) -> Option<u32> {
        self.active_tab().map(|tab| tab.id)
    }

    /// A single-tab session is pure terminal content: no title bar, no status
    /// bar, no tab strip. Only a real multi-tab session gets one chrome row.
    fn tabs_visible(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|frame| frame.tabs.len() > 1)
    }

    fn content_row(&self, row: u16) -> Option<u16> {
        let tabs_height = u16::from(self.tabs_visible());
        (row >= tabs_height).then(|| row - tabs_height)
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    fn send(&mut self, command: Command) -> Option<ResponsePayload> {
        match self.client.request(command) {
            Ok(response) => Some(response),
            Err(error) => {
                self.set_status(format!("FACT: {error}"));
                None
            }
        }
    }

    fn stop_events(&self) {
        self.events.cancel.store(true, Ordering::Release);
    }

    fn resize_session(&mut self) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let _ = self.send(Command::ResizeClient {
            cols: self.width,
            rows: self.height,
        });
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.overlay.is_some() {
            self.handle_overlay_key(key);
            return;
        }
        if self.scroll_mode.is_some() {
            self.handle_scroll_key(key);
            return;
        }
        let Some(chord) = keymap::chord_for(key) else {
            return;
        };

        if self.prefix_armed {
            self.prefix_armed = false;
            if chord == self.keys.prefix {
                // Prefix twice sends the chord itself, so apps that bind it
                // (PSReadLine's Ctrl+Space menu, for one) still receive it.
                self.forward_key(key);
                return;
            }
            if let Some(action) = self.keys.action_for(true, chord).map(str::to_string) {
                self.do_action(&action);
            } else {
                self.set_status(format!("Unbound key after prefix: {chord}"));
            }
            return;
        }

        if chord == self.keys.prefix {
            self.prefix_armed = true;
            return;
        }

        if let Some(action) = self.keys.action_for(false, chord).map(str::to_string) {
            self.do_action(&action);
            return;
        }

        if self.config.alt_shortcuts {
            if let Some(action) = direct_grid_action(key) {
                self.do_action(action);
                return;
            }
        }

        // Windows-terminal muscle memory: Ctrl+C copies a visible selection,
        // Ctrl+V pastes, and Ctrl/Shift+Insert are accepted as aliases.
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // The aliases follow their actions: `paste = "none"` or
        // `copy = "none"` hands these keys to the application as well.
        let paste = self.keys.binding("paste").is_some();
        let copy = self.keys.binding("copy").is_some();
        match key.code {
            CKeyCode::Esc if self.selection.is_some() => {
                self.selection = None;
                return;
            }
            CKeyCode::Char('v' | 'V') if paste && control && !shift && !alt => {
                self.paste_or_forward(key);
                return;
            }
            // Without selected text, Ctrl+C reaches the app as usual.
            CKeyCode::Char('c' | 'C') if copy && control && !shift && self.has_selection() => {
                self.copy_selection();
                self.selection = None;
                return;
            }
            CKeyCode::Insert if paste && shift => {
                self.paste_clipboard();
                return;
            }
            CKeyCode::Insert if copy && control => {
                self.copy_selection();
                self.selection = None;
                return;
            }
            _ => {}
        }

        self.forward_key(key);
    }

    fn forward_key(&mut self, key: KeyEvent) {
        self.selection = None;
        let Some(pane) = self.focused_pane() else {
            return;
        };
        if !pane.alive {
            self.set_status("That pane's process has exited. Restart it first.");
            return;
        }
        let app_cursor = pane.modes & pane_modes::APP_CURSOR != 0;
        let win32 = pane.modes & pane_modes::WIN32_INPUT != 0;
        let pane_id = pane.id;
        let kitty = pane.modes & pane_modes::KITTY_KEYS != 0;
        let encoded = match kitty.then(|| keymap::encode_key_kitty(key)).flatten() {
            Some(bytes) => Some(bytes),
            None if win32 => keymap::encode_key_win32(key),
            None => keymap::encode_key(key, app_cursor),
        };
        let Some(bytes) = encoded else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        let _ = self.send(Command::Input {
            pane: pane_id,
            data: bytes,
        });
    }

    fn paste_text(&mut self, text: String) {
        if self.scroll_mode.is_some() {
            self.set_status("Scroll mode: press Esc before pasting.");
            return;
        }
        if text.is_empty() {
            return;
        }
        self.selection = None;
        if text.len() > MAX_PASTE_BYTES {
            self.set_status(format!(
                "Clipboard is too large ({} MiB; limit {} MiB).",
                text.len() / (1024 * 1024),
                MAX_PASTE_BYTES / (1024 * 1024)
            ));
            return;
        }
        let Some(pane) = self.focused_id() else {
            return;
        };
        let _ = self.send(Command::Paste { pane, text });
    }

    fn do_action(&mut self, action: &str) {
        self.bells.clear();
        match action {
            "split_right" | "split_down" => {
                if let Some(pane) = self.focused_id() {
                    let direction = if action == "split_right" {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    };
                    let _ = self.send(Command::SplitPane { pane, direction });
                }
            }
            "close_pane" => {
                if let Some(pane) = self.focused_id() {
                    self.confirm_or_send(
                        format!(
                            "Close pane '{}'? Its process will be stopped.",
                            self.pane_label(pane)
                        ),
                        Command::ClosePane { pane },
                    );
                }
            }
            "close_tab" => {
                if let Some(tab) = self.active_tab() {
                    let tab_id = tab.id;
                    self.confirm_or_send(
                        format!("Terminate grid '{}' and stop all of its panes?", tab.title),
                        Command::CloseTab { tab: tab_id },
                    );
                }
            }
            "restart_pane" => {
                if let Some(pane) = self.focused_id() {
                    let _ = self.send(Command::RestartPane { pane });
                }
            }
            "zoom" => {
                // When the window is too small the engine falls back to
                // focused-pane mode without setting the tab's zoom flag. In
                // that state the useful toggle is "show all", not another
                // no-op maximize request.
                let tiny = self.frame.as_ref().is_some_and(|frame| frame.tiny_mode);
                let zoomed = !tiny && self.active_tab().is_some_and(|tab| !tab.zoomed);
                let _ = self.send(Command::ZoomPane { zoomed });
            }
            "maximize" => {
                let _ = self.send(Command::ZoomPane { zoomed: true });
            }
            "show_all" => {
                let _ = self.send(Command::ZoomPane { zoomed: false });
            }
            "minimize" => {
                if let Some(pane) = self.focused_id() {
                    let _ = self.send(Command::MinimizePane { pane });
                }
            }
            "restore" => {
                let _ = self.send(Command::RestorePanes);
            }
            "focus_left" | "focus_right" | "focus_up" | "focus_down" => {
                let direction = match action {
                    "focus_left" => fact_protocol::FocusDirection::Left,
                    "focus_right" => fact_protocol::FocusDirection::Right,
                    "focus_up" => fact_protocol::FocusDirection::Up,
                    _ => fact_protocol::FocusDirection::Down,
                };
                let _ = self.send(Command::FocusDirection { direction });
            }
            "next_pane" => {
                let _ = self.send(Command::NextPane);
            }
            "prev_pane" => {
                let _ = self.send(Command::PrevPane);
            }
            "new_tab" => {
                let _ = self.send(Command::NewTab {
                    profile: None,
                    cwd: None,
                });
            }
            "next_tab" => {
                let _ = self.send(Command::NextTab);
            }
            "prev_tab" => {
                let _ = self.send(Command::PrevTab);
            }
            "rename_tab" => {
                if let Some(tab) = self.active_tab() {
                    self.overlay = Some(Overlay::Prompt(PromptState {
                        title: format!("Rename tab '{}'", tab.title),
                        value: tab.title.clone(),
                        kind: PromptKind::RenameTab { tab: tab.id },
                    }));
                }
            }
            "palette" => {
                self.overlay = Some(Overlay::Palette(PaletteState {
                    query: String::new(),
                    selected: 0,
                }));
            }
            "search" => {
                if self.focused_id().is_some() {
                    self.overlay = Some(Overlay::Search(SearchState {
                        query: String::new(),
                        result: None,
                        selected: 0,
                        case_sensitive: false,
                    }));
                } else {
                    self.set_status("No pane to search.");
                }
            }
            "scroll_mode" => self.enter_scroll_mode(),
            "help" => {
                self.overlay = Some(Overlay::Help(HelpState::default()));
            }
            "quit" => {
                self.exit_message =
                    Some("Closing FACT. Every shell in this window has exited.".into());
            }
            "copy" => self.copy_selection(),
            "paste" => self.paste_clipboard(),
            "swap_pane" => {
                if let Some(frame) = &self.frame {
                    if let Some(focused) = frame.focus {
                        if let Some(next) = frame
                            .panes
                            .iter()
                            .map(|pane| pane.id)
                            .find(|id| *id != focused)
                        {
                            let _ = self.send(Command::SwapPanes {
                                a: focused,
                                b: next,
                            });
                        } else {
                            self.set_status("There is no other visible pane to swap with.");
                        }
                    }
                }
            }
            "move_pane" => {
                let pane = self.focused_id();
                let current_tab = self.active_tab_id();
                let target = self.frame.as_ref().and_then(|frame| {
                    frame
                        .tabs
                        .iter()
                        .find(|tab| Some(tab.id) != current_tab)
                        .map(|tab| tab.id)
                });
                match (pane, target) {
                    (Some(pane), Some(target_tab)) => {
                        let _ = self.send(Command::MovePaneToTab { pane, target_tab });
                    }
                    _ => self.set_status("There is no other tab to move this pane to."),
                }
            }
            "resize_left" | "resize_right" | "resize_up" | "resize_down" => {
                let direction = match action {
                    "resize_left" => fact_protocol::FocusDirection::Left,
                    "resize_right" => fact_protocol::FocusDirection::Right,
                    "resize_up" => fact_protocol::FocusDirection::Up,
                    _ => fact_protocol::FocusDirection::Down,
                };
                let _ = self.send(Command::ResizePane { direction });
            }
            "balance" => {
                if self.send(Command::BalancePanes).is_some() {
                    self.set_status("Panes balanced.");
                }
            }
            "cycle_theme" => {
                self.theme = self.theme.next();
                self.sync_wallpaper_base();
                // Programs that ask for the default colors get the new ones.
                let _ = self.send(Command::SetTheme {
                    name: self.theme.name.to_string(),
                });
                self.set_status(format!("Theme: {}", self.theme.name));
            }
            other => self.set_status(format!("Unknown action: {other}")),
        }
    }

    fn pane_label(&self, pane: u64) -> String {
        self.frame
            .as_ref()
            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
            .map(|view| friendly_title(&view.title))
            .unwrap_or_else(|| format!("pane {pane}"))
    }

    fn confirm_or_send(&mut self, message: String, command: Command) {
        if self.config.confirm_close {
            self.overlay = Some(Overlay::Confirm(ConfirmState { message, command }));
        } else {
            let _ = self.send(command);
        }
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selection_text() else {
            self.set_status("Select text with the mouse before copying.");
            return;
        };
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
            Ok(()) => self.set_status("Copied."),
            Err(error) => self.set_status(format!("Clipboard copy failed: {error}")),
        }
    }

    fn has_selection(&self) -> bool {
        self.selection_text().is_some()
    }

    /// Count consecutive clicks in the same cell, capped at three. Double and
    /// triple clicks select a word or a line like Windows Terminal.
    fn register_click(&mut self, pane: u64, x: u16, y: u16) -> u8 {
        let now = Instant::now();
        let count = match self.last_click {
            Some(previous)
                if previous.pane == pane
                    && previous.x == x
                    && previous.y == y
                    && now.duration_since(previous.at) <= DOUBLE_CLICK =>
            {
                (previous.count + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some(ClickTracker {
            at: now,
            pane,
            x,
            y,
            count,
        });
        count
    }

    fn select_word(&mut self, pane: u64, x: u16, y: u16) {
        let Some(buffer) = self.buffers.get(&pane).cloned() else {
            self.selection = None;
            return;
        };
        let (start, end) = buffer.word_bounds(x, y);
        self.selection = Some(Selection {
            pane,
            anchor: (start, y),
            end: (end, y),
            buffer: Some(buffer),
        });
    }

    fn select_line(&mut self, pane: u64, y: u16) {
        let Some(buffer) = self.buffers.get(&pane).cloned() else {
            self.selection = None;
            return;
        };
        let (start, end) = buffer.line_bounds();
        self.selection = Some(Selection {
            pane,
            anchor: (start, y),
            end: (end, y),
            buffer: Some(buffer),
        });
    }

    fn open_context_menu(&mut self, mouse: MouseEvent) {
        self.overlay = Some(Overlay::Context(ContextState {
            column: mouse.column,
            row: mouse.row,
            selected: 0,
            actions: vec![
                "split_right",
                "split_down",
                "maximize",
                "show_all",
                "zoom",
                "minimize",
                "new_tab",
                "rename_tab",
                "search",
                "restart_pane",
                "close_pane",
                "close_tab",
                "copy",
            ],
        }));
    }

    /// Ctrl+V pastes clipboard text. When the clipboard holds no text (an
    /// image or a file), the key goes to the app instead, so agents such as
    /// Claude Code and Codex can attach the image themselves.
    fn paste_or_forward(&mut self, key: KeyEvent) {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) if !text.is_empty() => self.paste_text(text),
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => self.forward_key(key),
            Err(error) => self.set_status(format!("Clipboard paste failed: {error}")),
        }
    }

    fn paste_clipboard(&mut self) {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) => {
                if text.is_empty() {
                    self.set_status("The clipboard is empty.");
                } else {
                    self.paste_text(text);
                }
            }
            Err(error) => self.set_status(format!("Clipboard paste failed: {error}")),
        }
    }

    fn selection_text(&self) -> Option<String> {
        selection_text(&self.buffers, self.selection.as_ref()?)
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        match overlay {
            Overlay::Help(mut help) => {
                let total = help_lines(&self.keys).len();
                let closes = matches!(key.code, CKeyCode::F(1) | CKeyCode::Char('q'))
                    || keymap::chord_for(key)
                        .is_some_and(|chord| self.keys.action_for(false, chord) == Some("help"));
                match key.code {
                    // Esc is reserved for stopping agent CLIs in the focused
                    // pane: it reaches the pane and help stays open. The help
                    // key (F1 by default) closes it.
                    _ if closes => {}
                    CKeyCode::Esc => {
                        self.overlay = Some(Overlay::Help(help));
                        self.forward_key(key);
                    }
                    CKeyCode::Up => {
                        help.scroll = help.scroll.saturating_sub(1);
                        self.overlay = Some(Overlay::Help(help));
                    }
                    CKeyCode::Down => {
                        help.scroll = (help.scroll + 1).min(total.saturating_sub(1));
                        self.overlay = Some(Overlay::Help(help));
                    }
                    CKeyCode::PageUp => {
                        help.scroll = help.scroll.saturating_sub(10);
                        self.overlay = Some(Overlay::Help(help));
                    }
                    CKeyCode::PageDown => {
                        help.scroll = (help.scroll + 10).min(total.saturating_sub(1));
                        self.overlay = Some(Overlay::Help(help));
                    }
                    CKeyCode::Home => {
                        help.scroll = 0;
                        self.overlay = Some(Overlay::Help(help));
                    }
                    CKeyCode::End => {
                        help.scroll = total.saturating_sub(1);
                        self.overlay = Some(Overlay::Help(help));
                    }
                    _ => self.overlay = Some(Overlay::Help(help)),
                }
            }
            Overlay::Palette(mut palette) => {
                let items = palette_items(&self.keys);
                match key.code {
                    CKeyCode::Esc => {}
                    CKeyCode::Up => {
                        palette.selected = palette.selected.saturating_sub(1);
                        self.overlay = Some(Overlay::Palette(palette));
                    }
                    CKeyCode::Down => {
                        let filtered = filtered_palette(&items, &palette.query).len();
                        if filtered > 0 {
                            palette.selected = (palette.selected + 1).min(filtered - 1);
                        }
                        self.overlay = Some(Overlay::Palette(palette));
                    }
                    CKeyCode::Enter => {
                        let filtered = filtered_palette(&items, &palette.query);
                        if let Some(item) = filtered.get(palette.selected) {
                            let action = item.action;
                            self.do_action(action);
                        }
                    }
                    CKeyCode::Backspace => {
                        palette.query.pop();
                        palette.selected = 0;
                        self.overlay = Some(Overlay::Palette(palette));
                    }
                    CKeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        palette.query.clear();
                        palette.selected = 0;
                        self.overlay = Some(Overlay::Palette(palette));
                    }
                    CKeyCode::Char(_) if keymap::typed_char(&key).is_some() => {
                        let c = keymap::typed_char(&key).unwrap_or_default();
                        if palette.query.chars().count() < 64 {
                            palette.query.push(c);
                        }
                        palette.selected = 0;
                        self.overlay = Some(Overlay::Palette(palette));
                    }
                    _ => self.overlay = Some(Overlay::Palette(palette)),
                }
            }
            Overlay::Search(mut search) => match key.code {
                CKeyCode::Esc => {}
                CKeyCode::Enter => {
                    if search.result.is_none() {
                        if search.query.is_empty() {
                            return;
                        }
                        self.run_search(&mut search);
                        self.overlay = Some(Overlay::Search(search));
                    } else {
                        let hit = search.result.as_ref().and_then(|result| {
                            result
                                .hits
                                .get(search.selected)
                                .map(|hit| (result.pane, hit.line))
                        });
                        if let Some((pane, line)) = hit {
                            self.focus(pane);
                            let _ = self.send(Command::ScrollToLine { pane, line });
                            self.set_status(format!("Jumped to line {line}."));
                        } else {
                            self.overlay = Some(Overlay::Search(search));
                        }
                    }
                }
                CKeyCode::Up => {
                    search.selected = search.selected.saturating_sub(1);
                    self.overlay = Some(Overlay::Search(search));
                }
                CKeyCode::Down => {
                    if let Some(result) = &search.result {
                        let max = result.hits.len().saturating_sub(1);
                        search.selected = (search.selected + 1).min(max);
                    }
                    self.overlay = Some(Overlay::Search(search));
                }
                CKeyCode::Backspace => {
                    search.result = None;
                    search.selected = 0;
                    search.query.pop();
                    self.overlay = Some(Overlay::Search(search));
                }
                CKeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    search.result = None;
                    search.selected = 0;
                    search.query.clear();
                    self.overlay = Some(Overlay::Search(search));
                }
                CKeyCode::Char(_) if keymap::typed_char(&key).is_some() => {
                    let c = keymap::typed_char(&key).unwrap_or_default();
                    if search.query.chars().count() < MAX_SEARCH_QUERY {
                        search.result = None;
                        search.selected = 0;
                        search.query.push(c);
                    }
                    self.overlay = Some(Overlay::Search(search));
                }
                _ => self.overlay = Some(Overlay::Search(search)),
            },
            Overlay::Prompt(mut prompt) => match key.code {
                CKeyCode::Esc => {}
                CKeyCode::Enter => {
                    if prompt.value.trim().is_empty() {
                        self.set_status("The name cannot be empty.");
                        self.overlay = Some(Overlay::Prompt(prompt));
                    } else {
                        let command = match prompt.kind {
                            PromptKind::RenameTab { tab } => Command::RenameTab {
                                tab,
                                title: prompt.value.trim().to_string(),
                            },
                        };
                        if self.send(command).is_some() {
                            self.set_status("Tab renamed.");
                        }
                    }
                }
                CKeyCode::Backspace => {
                    prompt.value.pop();
                    self.overlay = Some(Overlay::Prompt(prompt));
                }
                CKeyCode::Char(_) if keymap::typed_char(&key).is_some() => {
                    let c = keymap::typed_char(&key).unwrap_or_default();
                    if prompt.value.chars().count() < 60 {
                        prompt.value.push(c);
                    }
                    self.overlay = Some(Overlay::Prompt(prompt));
                }
                _ => self.overlay = Some(Overlay::Prompt(prompt)),
            },
            Overlay::Confirm(confirm) => match key.code {
                CKeyCode::Esc | CKeyCode::Char('n') | CKeyCode::Char('N') => {
                    self.set_status("Cancelled.");
                }
                CKeyCode::Enter | CKeyCode::Char('y') | CKeyCode::Char('Y') => {
                    let _ = self.send(confirm.command);
                }
                _ => self.overlay = Some(Overlay::Confirm(confirm)),
            },
            Overlay::Context(mut context) => match key.code {
                CKeyCode::Esc => {}
                CKeyCode::Up => {
                    context.selected = context.selected.saturating_sub(1);
                    self.overlay = Some(Overlay::Context(context));
                }
                CKeyCode::Down => {
                    context.selected = (context.selected + 1).min(context.actions.len() - 1);
                    self.overlay = Some(Overlay::Context(context));
                }
                CKeyCode::Enter => {
                    if let Some(action) = context.actions.get(context.selected) {
                        let action = *action;
                        self.do_action(action);
                    }
                }
                _ => self.overlay = Some(Overlay::Context(context)),
            },
        }
    }

    fn run_search(&mut self, search: &mut SearchState) {
        let Some(pane) = self.focused_id() else {
            self.set_status("No pane to search.");
            return;
        };
        let Some(ResponsePayload::Search(result)) = self.send(Command::SearchPane {
            pane,
            query: search.query.clone(),
            case_sensitive: search.case_sensitive,
            limit: 100,
        }) else {
            return;
        };
        if result.hits.is_empty() {
            self.set_status(format!("No matches for '{}'.", result.query));
        }
        search.selected = 0;
        search.result = Some(result);
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.mouse_position = Some((mouse.column, mouse.row));
        // Complete an app gesture even if release lands on the tab strip.
        if let MouseEventKind::Up(button) = mouse.kind {
            if let Some(pane) = self.app_mouse_down.release(button) {
                let content_y = mouse.row.saturating_sub(u16::from(self.tabs_visible()));
                if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                    self.forward_mouse_button(button, pane, x, y, false, mouse.modifiers);
                }
                return;
            }
        }
        if self.overlay.is_some() {
            self.handle_overlay_mouse(mouse);
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            if self.tab_drag.is_some() {
                self.drag_tab_to(mouse.column);
                return;
            }
            if self.pane_drag.is_some() {
                self.update_pane_drop(mouse.column, mouse.row);
                return;
            }
        }
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if let Some(drag) = self.pane_drag.take() {
                self.drop_pane(drag);
                return;
            }
            if self.tab_drag.take().is_some() {
                return;
            }
            if let Some(drag) = self.scroll_drag.take() {
                if let Some(content_y) = self.content_row(mouse.row) {
                    self.scroll_drag_to(&drag, content_y);
                }
                return;
            }
            if let Some(mut drag) = self.drag_split.take() {
                // Moves are throttled; the release lands exactly.
                if let Some(content_y) = self.content_row(mouse.row) {
                    drag.last_sent = Instant::now() - Duration::from_secs(1);
                    self.drag_split_to(drag, mouse.column, content_y);
                    self.drag_split = None;
                }
                return;
            }
            // Wherever the button comes up (the tab strip too), a selection
            // ends here.
            let dragged = self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.anchor != selection.end);
            if dragged && self.config.copy_on_select {
                self.copy_selection();
            } else if !dragged {
                self.selection = None;
            }
            return;
        }
        if self.tabs_visible() && mouse.row == 0 {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                if new_tab_rect(self.width).is_some_and(|rect| rect.contains(mouse.column, 0)) {
                    let _ = self.send(Command::NewTab {
                        profile: None,
                        cwd: None,
                    });
                    return;
                }
                if let Some(tab) = self.tab_at(mouse.column) {
                    let _ = self.send(Command::SelectTab { tab });
                    // Holding the button and moving along the strip reorders.
                    self.tab_drag = Some(tab);
                }
            }
            return;
        }
        let Some(content_y) = self.content_row(mouse.row) else {
            return;
        };
        let count = self
            .frame
            .as_ref()
            .map(|frame| frame.minimized.len())
            .unwrap_or(0);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && minimized_tray_rect(self.width, self.height, count)
                .is_some_and(|rect| rect.contains(mouse.column, mouse.row))
        {
            let _ = self.send(Command::RestorePanes);
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                self.app_mouse_down.release(MouseButton::Left);
                // Alt+drag grabs the whole pane, like moving a window.
                if mouse.modifiers.contains(KeyModifiers::ALT)
                    && !mouse.modifiers.contains(KeyModifiers::CONTROL)
                {
                    if let Some(pane) = self.pane_slot_at(mouse.column, content_y) {
                        self.focus(pane);
                        self.pane_drag = Some(PaneDrag { pane, target: None });
                        self.set_status("Move pane: drop it on another pane to swap, or on a tab.");
                    }
                    return;
                }
                if let Some(geometry) = self.scrollbar_at(mouse.column, content_y) {
                    self.focus(geometry.pane);
                    let on_thumb = content_y >= geometry.thumb_y
                        && content_y < geometry.thumb_y.saturating_add(geometry.thumb_height);
                    let base_offset = if on_thumb {
                        geometry.scroll_offset
                    } else {
                        let offset = scrollbar_jump_offset(&geometry, content_y);
                        let _ = self.send(Command::SetScrollOffset {
                            pane: geometry.pane,
                            offset,
                        });
                        offset
                    };
                    self.scroll_drag = Some(ScrollDrag {
                        geometry,
                        last_sent: Instant::now() - Duration::from_millis(100),
                        anchor_y: content_y,
                        base_offset,
                    });
                    return;
                }
                if let Some(handle) = self.split_at(mouse.column, content_y) {
                    let now = Instant::now();
                    let double = self.last_gutter_click.as_ref().is_some_and(|(path, at)| {
                        *path == handle.path && now.duration_since(*at) <= DOUBLE_CLICK
                    });
                    if double {
                        self.last_gutter_click = None;
                        if self.send(Command::BalancePanes).is_some() {
                            self.set_status("Panes balanced.");
                        }
                        return;
                    }
                    self.last_gutter_click = Some((handle.path.clone(), now));
                    self.drag_split = Some(handle);
                    return;
                }
                let Some(pane) = self.pane_slot_at(mouse.column, content_y) else {
                    self.selection = None;
                    return;
                };
                self.focus(pane);
                let Some((_pane, local_x, local_y)) = self.pane_at(mouse.column, content_y) else {
                    self.selection = None;
                    return;
                };
                let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                if let Some(view) = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                {
                    let app_mouse =
                        app_tracks_mouse(view.modes) && view.scroll_offset == 0 && !shift;
                    // Double clicks belong to apps that track the mouse;
                    // elsewhere they select a word, triple clicks a line.
                    let clicks = if app_mouse {
                        self.last_click = None;
                        1
                    } else {
                        self.register_click(pane, local_x, local_y)
                    };
                    if clicks >= 2 {
                        if clicks == 2 {
                            self.select_word(pane, local_x, local_y);
                        } else {
                            self.select_line(pane, local_y);
                        }
                        return;
                    }
                    if app_mouse {
                        self.app_mouse_down.press(MouseButton::Left, pane);
                        self.forward_mouse_button(
                            MouseButton::Left,
                            pane,
                            local_x,
                            local_y,
                            true,
                            mouse.modifiers,
                        );
                    } else {
                        self.selection = Some(Selection {
                            pane,
                            anchor: (local_x, local_y),
                            end: (local_x, local_y),
                            buffer: self.buffers.get(&pane).cloned(),
                        });
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    if let Some(pane) = self.pane_slot_at(mouse.column, content_y) {
                        self.focus(pane);
                    }
                    self.open_context_menu(mouse);
                    return;
                }
                // PowerShell and Windows Terminal muscle memory: right-click
                // copies the current selection, otherwise it pastes.
                if self.has_selection() {
                    self.copy_selection();
                    self.selection = None;
                    return;
                }
                let Some(pane) = self.pane_slot_at(mouse.column, content_y) else {
                    return;
                };
                self.focus(pane);
                let app_mouse = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                    .is_some_and(|view| app_tracks_mouse(view.modes) && view.scroll_offset == 0);
                if app_mouse {
                    if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                        self.selection = None;
                        self.app_mouse_down.press(MouseButton::Right, pane);
                        self.forward_mouse_button(
                            MouseButton::Right,
                            pane,
                            x,
                            y,
                            true,
                            mouse.modifiers,
                        );
                    }
                } else {
                    self.paste_clipboard();
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some((pane, x, y)) = self.pane_at(mouse.column, content_y) {
                    let tracks = self
                        .frame
                        .as_ref()
                        .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                        .is_some_and(|view| {
                            app_tracks_mouse(view.modes) && view.scroll_offset == 0
                        });
                    if tracks && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.selection = None;
                        self.app_mouse_down.press(MouseButton::Middle, pane);
                        self.forward_mouse_button(
                            MouseButton::Middle,
                            pane,
                            x,
                            y,
                            true,
                            mouse.modifiers,
                        );
                    }
                }
            }
            MouseEventKind::Drag(button) => {
                if button == MouseButton::Left {
                    if let Some(drag) = self.scroll_drag.clone() {
                        if drag.last_sent.elapsed() >= Duration::from_millis(35) {
                            self.scroll_drag_to(&drag, content_y);
                            if let Some(active) = &mut self.scroll_drag {
                                active.last_sent = Instant::now();
                            }
                        }
                        return;
                    }
                    if let Some(drag) = self.drag_split.clone() {
                        self.drag_split_to(drag, mouse.column, content_y);
                        return;
                    }
                }
                if let Some(pane) = self.app_mouse_down.pane(button) {
                    let drags = self
                        .frame
                        .as_ref()
                        .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                        .is_some_and(|view| {
                            view.modes & (pane_modes::MOUSE_DRAG | pane_modes::MOUSE_MOTION) != 0
                        });
                    if drags {
                        if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                            self.send_mouse(
                                pane,
                                MouseEventKind::Drag(button),
                                x,
                                y,
                                mouse.modifiers,
                            );
                        }
                    }
                    return;
                }
                if button == MouseButton::Left {
                    if let Some(pane) = self.selection.as_ref().map(|selection| selection.pane) {
                        if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                            self.selection.as_mut().unwrap().end = (x, y);
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.selection = None;
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                let delta = if up { 3 } else { -3 };
                if let Some(pane) = self.scroll_mode {
                    let mode = self
                        .frame
                        .as_ref()
                        .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                        .and_then(pane_scroll_mode);
                    match mode {
                        Some(ScrollMode::App) => self.send_app_wheel(pane, up),
                        Some(ScrollMode::History) => {
                            let _ = self.send(Command::ScrollPane { pane, delta });
                        }
                        None => {
                            self.scroll_mode = None;
                            self.set_status("The scrolled pane closed; scroll mode ended.");
                        }
                    }
                    return;
                }
                let Some((pane, x, y)) = self.pane_at(mouse.column, content_y) else {
                    return;
                };
                let (app_mouse, mode) = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                    .map(|view| {
                        (
                            app_tracks_mouse(view.modes) && view.scroll_offset == 0,
                            pane_scroll_mode(view),
                        )
                    })
                    .unwrap_or((false, None));
                if mode == Some(ScrollMode::App) {
                    if app_mouse {
                        let mut modifiers = mouse.modifiers;
                        modifiers.remove(KeyModifiers::SHIFT);
                        self.send_mouse(pane, mouse.kind, x, y, modifiers);
                    } else {
                        // The app owns the viewport but did not request mouse
                        // reporting. Use page keys to browse its transcript.
                        self.send_app_wheel(pane, up);
                    }
                } else if app_mouse && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    self.send_mouse(pane, mouse.kind, x, y, mouse.modifiers);
                } else {
                    let _ = self.send(Command::ScrollPane { pane, delta });
                }
            }
            MouseEventKind::Moved if !mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some((pane, x, y)) = self.pane_at(mouse.column, content_y) {
                    let motion = self
                        .frame
                        .as_ref()
                        .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                        .is_some_and(|view| {
                            view.modes & pane_modes::MOUSE_MOTION != 0 && view.scroll_offset == 0
                        });
                    if motion {
                        self.send_mouse(pane, mouse.kind, x, y, mouse.modifiers);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_overlay_mouse(&mut self, mouse: MouseEvent) {
        if matches!(
            self.overlay,
            Some(Overlay::Context(_) | Overlay::Help(_) | Overlay::Palette(_) | Overlay::Search(_))
        ) {
            let code = match mouse.kind {
                MouseEventKind::ScrollUp => Some(CKeyCode::Up),
                MouseEventKind::ScrollDown => Some(CKeyCode::Down),
                _ => None,
            };
            if let Some(code) = code {
                self.handle_overlay_key(KeyEvent::new(code, KeyModifiers::NONE));
                return;
            }
        }
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        if let Some(Overlay::Context(context)) = &mut self.overlay {
            let area = ratatui::layout::Rect::new(0, 0, self.width, self.height);
            let Some((rect, start)) = context.layout(area, &self.keys) else {
                self.overlay = None;
                return;
            };
            if mouse.column > rect.x
                && mouse.column < rect.right() - 1
                && mouse.row > rect.y
                && mouse.row < rect.bottom() - 1
            {
                let index = start + usize::from(mouse.row - rect.y - 1);
                if index < context.actions.len() {
                    context.selected = index;
                    self.handle_overlay_key(KeyEvent::new(CKeyCode::Enter, KeyModifiers::NONE));
                }
            } else if !rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) {
                self.overlay = None;
            }
        }
    }

    fn send_mouse(
        &mut self,
        pane: u64,
        kind: MouseEventKind,
        x: u16,
        y: u16,
        modifiers: KeyModifiers,
    ) {
        let modes = self
            .frame
            .as_ref()
            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
            .map_or(0, |view| view.modes);
        if !app_tracks_mouse(modes) {
            return;
        }
        if let Some(data) = keymap::encode_mouse(modes, kind, x, y, modifiers) {
            let _ = self.send(Command::Input { pane, data });
        }
    }

    fn forward_mouse_button(
        &mut self,
        button: MouseButton,
        pane: u64,
        x: u16,
        y: u16,
        pressed: bool,
        modifiers: KeyModifiers,
    ) {
        self.send_mouse(
            pane,
            if pressed {
                MouseEventKind::Down(button)
            } else {
                MouseEventKind::Up(button)
            },
            x,
            y,
            modifiers,
        );
    }

    fn visible_scrollbar(&self, pane: &PaneView) -> Option<ScrollbarGeometry> {
        if self.overlay.is_some() {
            return None;
        }
        let geometry = scrollbar_geometry(pane)?;
        let dragging = self
            .scroll_drag
            .as_ref()
            .is_some_and(|drag| drag.geometry.pane == pane.id);
        let hovering = self.selection.is_none()
            && scrollbar_hovered(
                &geometry,
                self.mouse_position,
                u16::from(self.tabs_visible()),
            );
        (dragging || hovering).then_some(geometry)
    }

    fn scrollbar_at(&self, column: u16, content_y: u16) -> Option<ScrollbarGeometry> {
        self.frame.as_ref()?.panes.iter().find_map(|pane| {
            self.visible_scrollbar(pane)
                .filter(|geometry| geometry.track.contains(column, content_y))
        })
    }

    fn scroll_drag_to(&mut self, drag: &ScrollDrag, content_y: u16) {
        self.selection = None;
        if !self.frame.as_ref().is_some_and(|frame| {
            frame
                .panes
                .iter()
                .any(|pane| pane.id == drag.geometry.pane && scrollbar_geometry(pane).is_some())
        }) {
            return;
        }
        let offset =
            scrollbar_drag_offset(&drag.geometry, drag.anchor_y, drag.base_offset, content_y);
        let _ = self.send(Command::SetScrollOffset {
            pane: drag.geometry.pane,
            offset,
        });
    }

    fn pane_at(&self, column: u16, content_y: u16) -> Option<(u64, u16, u16)> {
        let frame = self.frame.as_ref()?;
        for pane in &frame.panes {
            if pane.slot.contains(column, content_y) {
                let content_x = pane.slot.x;
                let content_y0 = pane.slot.y;
                if column >= content_x
                    && content_y >= content_y0
                    && column < content_x + pane.content_cols
                    && content_y < content_y0 + pane.content_rows
                {
                    return Some((
                        pane.id,
                        column.saturating_sub(content_x),
                        content_y.saturating_sub(content_y0),
                    ));
                }
                return None;
            }
        }
        None
    }

    fn pane_slot_at(&self, column: u16, content_y: u16) -> Option<u64> {
        let frame = self.frame.as_ref()?;
        frame
            .panes
            .iter()
            .find(|pane| pane.slot.contains(column, content_y))
            .map(|pane| pane.id)
    }

    /// Map a screen position into a pane's content grid, clamping drag and
    /// release events that travel outside the pane so mouse-capturing programs
    /// always receive a matching release.
    fn pane_local(&self, pane: u64, column: u16, content_y: u16) -> Option<(u16, u16)> {
        let frame = self.frame.as_ref()?;
        let view = frame.panes.iter().find(|view| view.id == pane)?;
        if view.content_cols == 0 || view.content_rows == 0 {
            return None;
        }
        let origin_x = view.slot.x;
        let origin_y = view.slot.y;
        let x = column
            .clamp(origin_x, origin_x + view.content_cols - 1)
            .saturating_sub(origin_x);
        let y = content_y
            .clamp(origin_y, origin_y + view.content_rows - 1)
            .saturating_sub(origin_y);
        Some((x, y))
    }

    fn tab_at(&self, column: u16) -> Option<u32> {
        let frame = self.frame.as_ref()?;
        tab_ranges(&frame.tabs, self.width)
            .into_iter()
            .find(|tab| tab.rect.contains(column, 0))
            .map(|tab| tab.id)
    }

    fn split_at(&self, column: u16, content_y: u16) -> Option<DragSplit> {
        let frame = self.frame.as_ref()?;
        for split in frame.splits.iter().rev() {
            let region = split.region;
            match split.orientation {
                SplitOrientation::Horizontal => {
                    let usable = region.width.saturating_sub(1).max(2);
                    let left = ((usable as f32) * split.ratio).round() as u16;
                    let left = left.clamp(1, usable.saturating_sub(1));
                    let gutter = region.x + left;
                    if column.abs_diff(gutter) <= 1
                        && content_y >= region.y
                        && content_y < region.y + region.height
                    {
                        return Some(DragSplit {
                            path: split.path.clone(),
                            orientation: split.orientation,
                            region,
                            last_sent: Instant::now() - Duration::from_millis(100),
                        });
                    }
                }
                SplitOrientation::Vertical => {
                    let usable = region.height.saturating_sub(1).max(2);
                    let top = ((usable as f32) * split.ratio).round() as u16;
                    let top = top.clamp(1, usable.saturating_sub(1));
                    let gutter = region.y + top;
                    if content_y.abs_diff(gutter) <= 1
                        && column >= region.x
                        && column < region.x + region.width
                    {
                        return Some(DragSplit {
                            path: split.path.clone(),
                            orientation: split.orientation,
                            region,
                            last_sent: Instant::now() - Duration::from_millis(100),
                        });
                    }
                }
            }
        }
        None
    }

    /// Move the dragged tab to the position of the tab under the pointer.
    fn drag_tab_to(&mut self, column: u16) {
        let Some(tab) = self.tab_drag else {
            return;
        };
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        let Some(hovered) = tab_ranges(&frame.tabs, self.width)
            .into_iter()
            .find(|range| range.rect.contains(column, 0) && range.id != tab)
        else {
            return;
        };
        let position = |id: u32| frame.tabs.iter().position(|view| view.id == id);
        let (Some(from), Some(index)) = (position(tab), position(hovered.id)) else {
            return;
        };
        // Swap only past the middle of the other tab, so tabs of different
        // widths do not trade places back and forth under the pointer.
        let middle = hovered.rect.x + hovered.rect.width / 2;
        let past_middle = if index > from {
            column >= middle
        } else {
            column <= middle
        };
        if past_middle {
            let _ = self.send(Command::MoveTab { tab, index });
        }
    }

    fn pane_tab(&self, pane: u64) -> Option<u32> {
        self.frame
            .as_ref()?
            .panes
            .iter()
            .find(|view| view.id == pane)
            .map(|view| view.tab)
    }

    fn update_pane_drop(&mut self, column: u16, row: u16) {
        let Some(drag) = self.pane_drag else {
            return;
        };
        let target = if self.tabs_visible() && row == 0 {
            self.tab_at(column)
                .filter(|tab| Some(*tab) != self.pane_tab(drag.pane))
                .map(PaneDropTarget::Tab)
        } else {
            self.content_row(row)
                .and_then(|y| self.pane_slot_at(column, y))
                .filter(|pane| *pane != drag.pane)
                .map(PaneDropTarget::Pane)
        };
        self.pane_drag = Some(PaneDrag { target, ..drag });
    }

    fn drop_pane(&mut self, drag: PaneDrag) {
        match drag.target {
            Some(PaneDropTarget::Pane(other)) => {
                if self
                    .send(Command::SwapPanes {
                        a: drag.pane,
                        b: other,
                    })
                    .is_some()
                {
                    self.set_status("Panes swapped.");
                }
            }
            Some(PaneDropTarget::Tab(target_tab)) => {
                if self
                    .send(Command::MovePaneToTab {
                        pane: drag.pane,
                        target_tab,
                    })
                    .is_some()
                {
                    self.set_status("Pane moved to the tab.");
                }
            }
            None => self.status = None,
        }
    }

    fn drag_split_to(&mut self, mut drag: DragSplit, column: u16, content_y: u16) {
        let ratio = match drag.orientation {
            SplitOrientation::Horizontal => {
                let usable = drag.region.width.saturating_sub(1).max(2);
                (column.saturating_sub(drag.region.x)) as f32 / usable as f32
            }
            SplitOrientation::Vertical => {
                let usable = drag.region.height.saturating_sub(1).max(2);
                (content_y.saturating_sub(drag.region.y)) as f32 / usable as f32
            }
        }
        .clamp(fact_layout::MIN_RATIO, fact_layout::MAX_RATIO);
        if drag.last_sent.elapsed() >= Duration::from_millis(35) {
            let _ = self.send(Command::DragSplit {
                path: drag.path.clone(),
                ratio,
            });
            drag.last_sent = Instant::now();
        }
        self.drag_split = Some(drag);
    }
}

/// The key press FACT acts on, or `None` to skip the event. Console alt
/// codes (Alt+numpad, and pasted characters the keyboard layout cannot
/// type) arrive as a release with no press; other releases are skipped.
fn key_press(held: &mut HashSet<char>, key: KeyEvent) -> Option<KeyEvent> {
    let CKeyCode::Char(c) = key.code else {
        return (key.kind != KeyEventKind::Release).then_some(key);
    };
    let folded = c.to_lowercase().next().unwrap_or(c);
    if key.kind != KeyEventKind::Release {
        held.insert(folded);
        return Some(key);
    }
    if held.is_empty() {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        return Some(KeyEvent::new(CKeyCode::Char(c), KeyModifiers::NONE));
    }
    // The release of a held key, possibly with other modifiers by now
    // (Shift or AltGr let go first).
    if !held.remove(&folded) {
        let any = held.iter().next().copied();
        if let Some(any) = any {
            held.remove(&any);
        }
    }
    None
}

/// Keys that type text: characters (with Shift at most), Enter and Tab.
fn is_text_key(key: &KeyEvent) -> bool {
    match key.code {
        CKeyCode::Char(_) => (key.modifiers - KeyModifiers::SHIFT).is_empty(),
        CKeyCode::Enter | CKeyCode::Tab => key.modifiers.is_empty(),
        _ => false,
    }
}

/// Text keystrokes (including alt-code releases) at the end of a batch.
fn trailing_text_keys(batch: &[UiEvent]) -> usize {
    batch
        .iter()
        .rev()
        .filter(|event| !matches!(event, UiEvent::Message(_)))
        .take_while(|event| match event {
            UiEvent::Key(key, _) => {
                matches!(
                    key.code,
                    CKeyCode::Char(_) | CKeyCode::Enter | CKeyCode::Tab
                ) && !key.modifiers.contains(KeyModifiers::CONTROL)
            }
            _ => false,
        })
        .count()
}

/// The text of keystrokes that can only be a paste: a line break followed
/// by more text. Typing a command and Enter stays keystrokes.
fn pasted_text(keys: &[KeyEvent]) -> Option<String> {
    let enter = keys.iter().position(|key| key.code == CKeyCode::Enter)?;
    if keys.len() < 3 || enter + 1 == keys.len() {
        return None;
    }
    let text = keys
        .iter()
        .filter_map(|key| match key.code {
            CKeyCode::Char(c) => Some(c),
            CKeyCode::Enter => Some('\n'),
            CKeyCode::Tab => Some('\t'),
            _ => None,
        })
        .collect();
    Some(text)
}

fn palette_items(keys: &CompiledKeybinds) -> Vec<PaletteItem> {
    ACTION_INFO
        .iter()
        .map(|(action, label)| PaletteItem {
            action,
            label,
            shortcut: keys.describe(action),
        })
        .collect()
}

fn filtered_palette<'a>(items: &'a [PaletteItem], query: &str) -> Vec<&'a PaletteItem> {
    let query = query.trim().to_ascii_lowercase();
    items
        .iter()
        .filter(|item| {
            query.is_empty()
                || item.label.to_ascii_lowercase().contains(&query)
                || item.action.contains(&query)
        })
        .collect()
}

/// Direct grid aliases. Ctrl+M is indistinguishable from Enter in terminal
/// input, so Alt is the safe modifier for a direct chord. `Alt+T` terminates the
/// active grid; the remappable prefix bindings do the same things.
fn direct_grid_action(key: KeyEvent) -> Option<&'static str> {
    if !key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        CKeyCode::Char('m' | 'M') => Some("maximize"),
        CKeyCode::Char('n' | 'N') => Some("show_all"),
        CKeyCode::Char('t' | 'T') => Some("close_tab"),
        _ => None,
    }
}

pub(crate) fn help_lines(keys: &CompiledKeybinds) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = ACTION_INFO
        .iter()
        .map(|(action, label)| (label.to_string(), keys.describe(action)))
        .collect();
    lines.push((
        "Resize split".into(),
        "drag the gutter; double-click it to balance".into(),
    ));
    lines.push((
        "Move a pane".into(),
        "Alt+drag it onto another pane (swap) or a tab".into(),
    ));
    lines.push(("Reorder tabs".into(), "drag a tab along the strip".into()));
    lines.push((
        "Send the prefix itself".into(),
        format!("{} twice", keys.prefix),
    ));
    lines.push((
        "New tab".into(),
        "prefix+C, or click + when the tab strip is visible".into(),
    ));
    lines.push((
        "Scrollback / app scroll".into(),
        format!(
            "wheel or Shift+wheel; {} then arrows / PgUp / PgDn",
            keys.describe("scroll_mode")
        ),
    ));
    lines.push((
        "Bell".into(),
        "an amber dot appears at the pane's top-right".into(),
    ));
    lines.push((
        "Clipboard".into(),
        "select then right-click or Ctrl+C · right-click or Ctrl+V pastes".into(),
    ));
    lines.push((
        "Select text".into(),
        "drag, double-click for a word, triple-click for a line".into(),
    ));
    lines.push((
        "Context menu".into(),
        "Shift+right-click (right-click otherwise copies or pastes)".into(),
    ));
    lines.push((
        "Direct maximize / show all / close tab".into(),
        "Alt+M / Alt+N / Alt+T with alt_shortcuts = true".into(),
    ));
    lines.push(("Prefix".into(), keys.prefix.to_string()));
    lines
}

pub(crate) fn fit_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".into();
    }
    let mut result: String = text.chars().take(width - 1).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fact_protocol::ColorDto;

    fn cell(ch: char) -> CellDto {
        CellDto {
            ch,
            zero: Vec::new(),
            fg: ColorDto::Named(16),
            bg: ColorDto::Named(17),
            flags: 0,
        }
    }

    fn test_pane(buffer: &mut HashMap<u64, PaneBuffer>) {
        buffer.insert(
            1,
            PaneBuffer {
                cols: 5,
                rows: 2,
                cells: vec![
                    cell('a'),
                    cell('b'),
                    cell('c'),
                    cell('d'),
                    cell('e'),
                    cell('1'),
                    cell('2'),
                    cell('3'),
                    cell('4'),
                    cell('5'),
                ],
            },
        );
    }

    #[test]
    fn selection_normalizes_and_extracts_text() {
        let mut buffers = HashMap::new();
        test_pane(&mut buffers);
        let selection = Selection {
            pane: 1,
            anchor: (3, 0),
            end: (1, 1),
            buffer: None,
        };
        assert!(selection.contains(3, 0));
        assert!(selection.contains(0, 1));
        assert!(selection.contains(4, 0));
        assert!(!selection.contains(4, 1));
        assert_eq!(selection_text(&buffers, &selection).unwrap(), "de\n12");
    }

    #[test]
    fn selection_skips_wide_spacers_and_masks_hidden_cells() {
        let mut buffers = HashMap::new();
        let mut wide = cell('界');
        wide.flags = cell_flags::WIDE_CHAR;
        let mut spacer = cell(' ');
        spacer.flags = cell_flags::WIDE_SPACER;
        let mut hidden = cell('x');
        hidden.flags = cell_flags::HIDDEN;
        buffers.insert(
            1,
            PaneBuffer {
                cols: 4,
                rows: 1,
                cells: vec![wide, spacer, hidden, cell('!')],
            },
        );
        let selection = Selection {
            pane: 1,
            anchor: (0, 0),
            end: (3, 0),
            buffer: None,
        };
        assert_eq!(selection_text(&buffers, &selection).unwrap(), "界 !");
    }

    #[test]
    fn copying_uses_the_selected_snapshot_while_output_changes() {
        let mut buffers = HashMap::new();
        test_pane(&mut buffers);
        let selection = Selection {
            pane: 1,
            anchor: (0, 0),
            end: (4, 0),
            buffer: buffers.get(&1).cloned(),
        };
        buffers.get_mut(&1).unwrap().cells.fill(cell('x'));
        assert_eq!(selection_text(&buffers, &selection).unwrap(), "abcde");
    }

    #[test]
    fn overlapping_mouse_buttons_retain_their_original_panes() {
        let mut capture = AppMouseCapture::default();
        capture.press(MouseButton::Right, 7);
        capture.press(MouseButton::Middle, 8);
        capture.release(MouseButton::Left); // A local selection or rail press.
        capture.press(MouseButton::Left, 9);
        assert_eq!(capture.release(MouseButton::Right), Some(7));
        assert_eq!(capture.pane(MouseButton::Middle), Some(8));
        assert_eq!(capture.release(MouseButton::Left), Some(9));
        assert_eq!(capture.release(MouseButton::Middle), Some(8));
        assert_eq!(capture.release(MouseButton::Right), None);
    }

    fn press(code: CKeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keystrokes_with_a_line_break_inside_are_a_paste() {
        let keys = |text: &str| -> Vec<KeyEvent> {
            text.chars()
                .map(|c| match c {
                    '\n' => press(CKeyCode::Enter),
                    '\t' => press(CKeyCode::Tab),
                    c => press(CKeyCode::Char(c)),
                })
                .collect()
        };
        assert_eq!(pasted_text(&keys("ab\ncd")).as_deref(), Some("ab\ncd"));
        assert_eq!(
            pasted_text(&keys("a\tb\nc\n")).as_deref(),
            Some("a\tb\nc\n")
        );
        // A command and Enter, typed quickly, still runs.
        assert_eq!(pasted_text(&keys("ls\n")), None);
        assert_eq!(pasted_text(&keys("hello")), None);
        assert!(is_text_key(&KeyEvent::new(
            CKeyCode::Char('A'),
            KeyModifiers::SHIFT
        )));
        assert!(!is_text_key(&KeyEvent::new(
            CKeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_text_key(&KeyEvent::new(
            CKeyCode::Enter,
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn alt_codes_type_and_key_releases_do_not() {
        let release = |c: char, modifiers: KeyModifiers| {
            KeyEvent::new_with_kind(CKeyCode::Char(c), modifiers, KeyEventKind::Release)
        };
        let mut held = HashSet::new();
        // A release with no press: an alt code such as Alt+0233.
        let typed = key_press(&mut held, release('é', KeyModifiers::ALT)).unwrap();
        assert_eq!(typed.code, CKeyCode::Char('é'));
        assert_eq!(typed.modifiers, KeyModifiers::NONE);
        assert_eq!(typed.kind, KeyEventKind::Press);
        // An ordinary key: press types, release does not.
        let shifted = KeyEvent::new(CKeyCode::Char('A'), KeyModifiers::SHIFT);
        assert!(key_press(&mut held, shifted).is_some());
        // Shift let go first: the release reports 'a'.
        assert!(key_press(&mut held, release('a', KeyModifiers::NONE)).is_none());
        assert!(held.is_empty());
        // AltGr+Q types '@'; letting AltGr go first releases 'q'.
        let at = KeyEvent::new(
            CKeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(key_press(&mut held, at).is_some());
        assert!(key_press(&mut held, release('q', KeyModifiers::NONE)).is_none());
        assert!(held.is_empty());
        let enter_up =
            KeyEvent::new_with_kind(CKeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(key_press(&mut held, enter_up).is_none());
    }

    #[test]
    fn altgr_characters_are_text_not_shortcuts() {
        let altgr = KeyEvent::new(
            CKeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        if cfg!(windows) {
            let chord = keymap::chord_for(altgr).unwrap();
            assert!(!chord.mods.control && !chord.mods.alt);
            assert_eq!(keymap::typed_char(&altgr), Some('@'));
        }
        let shortcut = KeyEvent::new(CKeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(keymap::typed_char(&shortcut), None);
        let plain = KeyEvent::new(CKeyCode::Char('F'), KeyModifiers::SHIFT);
        assert_eq!(keymap::typed_char(&plain), Some('F'));
    }

    #[test]
    fn alt_grid_shortcuts_are_direct_actions() {
        let alt_m = KeyEvent::new(CKeyCode::Char('m'), KeyModifiers::ALT);
        let alt_n = KeyEvent::new(CKeyCode::Char('n'), KeyModifiers::ALT);
        let alt_t = KeyEvent::new(CKeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(direct_grid_action(alt_m), Some("maximize"));
        assert_eq!(direct_grid_action(alt_n), Some("show_all"));
        assert_eq!(direct_grid_action(alt_t), Some("close_tab"));
        assert_eq!(
            direct_grid_action(KeyEvent::new(CKeyCode::Char('t'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            direct_grid_action(KeyEvent::new(CKeyCode::Char('m'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            direct_grid_action(KeyEvent::new(
                CKeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            None
        );
    }

    #[test]
    fn every_configurable_action_has_a_label() {
        use std::collections::BTreeSet;
        let configurable: BTreeSet<_> = fact_config::keys::ACTIONS.iter().copied().collect();
        let labelled: BTreeSet<_> = ACTION_INFO.iter().map(|(action, _)| *action).collect();
        assert_eq!(labelled, configurable);
        assert_eq!(labelled.len(), ACTION_INFO.len(), "duplicate action label");
        for action in ACTION_INFO.iter().map(|(action, _)| *action) {
            assert_ne!(action_label(action), "Unknown action");
        }
    }

    #[test]
    fn word_selection_respects_delimiters() {
        let mut buffers = HashMap::new();
        let mut cells = Vec::new();
        for ch in "git status --short".chars() {
            cells.push(cell(ch));
        }
        buffers.insert(
            1,
            PaneBuffer {
                cols: cells.len() as u16,
                rows: 1,
                cells,
            },
        );
        let buffer = buffers.get(&1).unwrap();
        assert_eq!(buffer.word_bounds(0, 0), (0, 2));
        assert_eq!(buffer.word_bounds(5, 0), (4, 9));
        assert_eq!(buffer.word_bounds(10, 0), (10, 10));
        assert_eq!(buffer.line_bounds(), (0, 17));
    }

    fn test_view(width: u16) -> PaneView {
        PaneView {
            id: 7,
            tab: 1,
            title: "PowerShell".into(),
            pid: 42,
            alive: true,
            exit_code: None,
            slot: RectDto {
                x: 10,
                y: 2,
                width,
                height: 11,
            },
            content_cols: width,
            content_rows: 10,
            version: 1,
            cells: None,
            cursor: fact_protocol::CursorDto {
                x: 0,
                y: 0,
                visible: true,
                shape: fact_protocol::CursorShape::Block,
            },
            modes: 0,
            scroll_offset: 0,
            history_lines: 0,
            bell_seq: 0,
            focused: true,
        }
    }

    #[test]
    fn tabs_and_new_tab_button_do_not_overlap() {
        let tabs = vec![
            TabView {
                id: 1,
                title: "PowerShell".into(),
                active: true,
                pane_count: 1,
                zoomed: false,
            },
            TabView {
                id: 2,
                title: "Bash".into(),
                active: false,
                pane_count: 1,
                zoomed: false,
            },
        ];
        let ranges = tab_ranges(&tabs, 40);
        assert_eq!(ranges.len(), 2);
        let plus = new_tab_rect(40).unwrap();
        for range in &ranges {
            assert!(range.rect.x + range.rect.width <= plus.x);
        }
        assert!(ranges[0].label.contains("PowerShell"));
        assert!(ranges[1].label.contains("Bash"));
    }

    #[test]
    fn active_tab_stays_visible_when_the_strip_overflows() {
        let tabs: Vec<TabView> = (1..=10)
            .map(|id| TabView {
                id,
                title: format!("Tab {id}"),
                active: id == 10,
                pane_count: 1,
                zoomed: false,
            })
            .collect();
        let ranges = tab_ranges(&tabs, 40);
        let active = ranges
            .iter()
            .find(|range| range.id == 10)
            .expect("the active tab must stay clickable");
        assert!(active.rect.width >= 4);
        let plus = new_tab_rect(40).unwrap();
        for range in &ranges {
            assert!(range.rect.x + range.rect.width <= plus.x);
        }
    }

    #[test]
    fn minimized_tray_is_right_aligned_on_the_last_row() {
        let tray = minimized_tray_rect(10, 4, 2).unwrap();
        assert_eq!(tray.x + tray.width, 10);
        assert_eq!(tray.y, 3);
        assert!(tray.contains(6, 3));
        assert!(tray.contains(9, 3));
        assert!(!tray.contains(5, 3));
        assert!(!tray.contains(9, 2));
    }

    #[test]
    fn history_scrollbar_only_appears_near_the_right_edge() {
        let mut view = test_view(40);
        view.history_lines = 0;
        assert!(scrollbar_geometry(&view).is_none());
        view.history_lines = 2;
        let geometry = scrollbar_geometry(&view).unwrap();
        assert_eq!(geometry.max_offset, 2);
        assert!(geometry.thumb_height < geometry.track.height);
        assert!(!scrollbar_hovered(&geometry, None, 0));
        assert!(!scrollbar_hovered(&geometry, Some((20, 3)), 0));
        assert!(scrollbar_hovered(&geometry, Some((48, 3)), 0));
        assert!(scrollbar_hovered(&geometry, Some((49, 3)), 0));
        assert!(!scrollbar_hovered(&geometry, Some((50, 3)), 0));
        assert!(!scrollbar_hovered(&geometry, Some((49, 1)), 1));
        assert!(scrollbar_hovered(&geometry, Some((49, 3)), 1));
    }

    #[test]
    fn app_panes_keep_scroll_routing_without_a_fact_scrollbar() {
        let mut view = test_view(40);
        view.history_lines = 100;
        for modes in [
            pane_modes::ALT_SCREEN,
            pane_modes::MOUSE_CLICK,
            pane_modes::MOUSE_DRAG,
            pane_modes::MOUSE_MOTION,
        ] {
            view.modes = modes;
            assert!(scrollbar_geometry(&view).is_none());
        }
        view.modes = pane_modes::ALT_SCREEN;
        assert_eq!(pane_scroll_mode(&view), Some(ScrollMode::App));
    }

    #[test]
    fn scrollbar_maps_pointer_to_scrollback_offset() {
        let mut view = test_view(40);
        view.history_lines = 100;
        view.content_rows = 10;
        view.slot.height = 11;
        let bottom = scrollbar_geometry(&view).unwrap();
        assert_eq!(bottom.track.x, 49);
        assert_eq!(bottom.track.y, 2);
        assert_eq!(bottom.track.height, 10);
        assert_eq!(bottom.max_offset, 100);
        assert_eq!(scrollbar_jump_offset(&bottom, bottom.track.y), 100);
        assert_eq!(
            scrollbar_jump_offset(&bottom, bottom.track.y + bottom.track.height - 1),
            0
        );
        view.scroll_offset = 50;
        let middle = scrollbar_geometry(&view).unwrap();
        let anchor = middle.thumb_y;
        assert_eq!(scrollbar_drag_offset(&middle, anchor, 50, anchor), 50);
        assert!(scrollbar_drag_offset(&middle, anchor, 50, anchor + 1) < 50);
        view.scroll_offset = 100;
        let top = scrollbar_geometry(&view).unwrap();
        assert_eq!(top.thumb_y, top.track.y);
    }
}
