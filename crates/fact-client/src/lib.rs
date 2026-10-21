//! FACT terminal user interface.
//!
//! The daemon owns the shells and terminal state. This crate only renders the
//! frames it receives and forwards keyboard, mouse and clipboard input. It never
//! parses terminal output itself.

mod keymap;
mod render;
mod theme;

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode as CKeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use fact_config::{CompiledKeybinds, Config};
use fact_protocol::{
    cell_flags, friendly_title, pane_modes, AttachInfo, CellDto, Command, Frame, PaneView, RectDto,
    ResponsePayload, SearchResult, SessionSummary, SplitDirection, SplitOrientation, TabView,
    MAX_PASTE_BYTES, MAX_SEARCH_QUERY,
};
use fact_runtime::{DaemonClient, EventStream};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use theme::Theme;

const FRAME_POLL: Duration = Duration::from_millis(30);
const EVENT_POLL: Duration = Duration::from_millis(10);
const STATUS_LIFETIME: Duration = Duration::from_secs(2);
const MAX_EVENTS_PER_TURN: usize = 64;

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
    ("sessions", "Switch session"),
    ("detach", "Detach and keep shells"),
    ("help", "Help"),
    ("copy", "Copy selection"),
    ("paste", "Paste clipboard"),
    ("quit", "Close FACT UI"),
    ("swap_pane", "Swap with next pane"),
    ("move_pane", "Move pane to next tab"),
];

pub(crate) fn action_label(action: &str) -> &'static str {
    ACTION_INFO
        .iter()
        .find(|(id, _)| *id == action)
        .map(|(_, label)| *label)
        .unwrap_or("Unknown action")
}

/// Run the interactive terminal UI. Terminal state is always restored before
/// returning, including on errors.
pub fn run(client: DaemonClient, info: AttachInfo, config: Config) -> Result<()> {
    let keys = config
        .compiled_keybinds()
        .context("could not compile keyboard bindings")?;
    let mut terminal = TerminalGuard::enter(config.mouse)?;
    let size = terminal.size().context("could not read terminal size")?;
    let stream = client
        .subscribe(&info.stream_token)
        .context("could not subscribe to the session event stream")?;
    let events = spawn_reader(stream);

    let mut app = App::new(client, info, config, keys, events, size.width, size.height);
    app.resize_session();
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

fn spawn_reader(mut stream: EventStream) -> UiEvents {
    // Preserve ordered delta frames without letting a busy agent grow an
    // unbounded queue when drawing or clipboard work temporarily falls behind.
    let (tx, rx) = mpsc::sync_channel(8);
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    thread::Builder::new()
        .name("fact-ui-events".into())
        .spawn(move || loop {
            if thread_cancel.load(Ordering::Acquire) {
                break;
            }
            match stream.available() {
                Ok(false) => thread::sleep(EVENT_POLL),
                Ok(true) => match stream.recv() {
                    Ok(message) => {
                        if tx.send(UiEvent::Message(Box::new(message))).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(UiEvent::Closed(error.to_string()));
                        break;
                    }
                },
                Err(error) => {
                    let _ = tx.send(UiEvent::Closed(error.to_string()));
                    break;
                }
            }
        })
        .expect("spawning the event reader thread cannot fail");
    UiEvents {
        receiver: rx,
        cancel,
    }
}

enum UiEvent {
    Message(Box<fact_protocol::ServerMessage>),
    Closed(String),
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
            execute!(stdout, EnterAlternateScreen, Hide, EnableBracketedPaste)?;
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
    let _ = execute!(stdout, DisableBracketedPaste);
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
    Sessions(SessionsState),
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

#[derive(Debug)]
struct SessionsState {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

pub(crate) struct PaletteItem {
    pub action: &'static str,
    pub label: &'static str,
    pub shortcut: String,
}

struct App {
    client: DaemonClient,
    info: AttachInfo,
    config: Config,
    keys: CompiledKeybinds,
    theme: Theme,
    events: UiEvents,
    frame: Option<Frame>,
    buffers: HashMap<u64, PaneBuffer>,
    last_seq: u64,
    detach_base: Option<u64>,
    last_bells: HashMap<u64, u64>,
    bells: HashSet<u64>,
    width: u16,
    height: u16,
    overlay: Option<Overlay>,
    prefix_armed: bool,
    selection: Option<Selection>,
    drag_split: Option<DragSplit>,
    scroll_drag: Option<ScrollDrag>,
    mouse_position: Option<(u16, u16)>,
    scroll_mode: Option<u64>,
    app_mouse_down: AppMouseCapture,
    status: Option<(String, Instant)>,
    exit_message: Option<String>,
    fatal: Option<anyhow::Error>,
}

impl App {
    fn new(
        client: DaemonClient,
        info: AttachInfo,
        config: Config,
        keys: CompiledKeybinds,
        events: UiEvents,
        width: u16,
        height: u16,
    ) -> Self {
        let theme = Theme::by_name(&config.theme);
        Self {
            client,
            info,
            config,
            keys,
            theme,
            events,
            frame: None,
            buffers: HashMap::new(),
            last_seq: 0,
            detach_base: None,
            last_bells: HashMap::new(),
            bells: HashSet::new(),
            width,
            height,
            overlay: None,
            prefix_armed: false,
            selection: None,
            drag_split: None,
            scroll_drag: None,
            mouse_position: None,
            scroll_mode: None,
            app_mouse_down: AppMouseCapture::default(),
            status: None,
            exit_message: None,
            fatal: None,
        }
    }

    fn event_loop(&mut self, terminal: &mut TerminalGuard) -> Result<()> {
        self.resize_session();
        loop {
            self.drain_events();
            if self.fatal.is_some() || self.exit_message.is_some() {
                break;
            }

            let input_started = Instant::now();
            for index in 0..MAX_EVENTS_PER_TURN {
                let timeout = if index == 0 {
                    FRAME_POLL
                } else {
                    Duration::ZERO
                };
                if !event::poll(timeout).context("could not poll terminal input")? {
                    break;
                }
                match event::read().context("could not read terminal input")? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        self.handle_key(key);
                    }
                    Event::Mouse(mouse) if self.config.mouse => {
                        self.handle_mouse(mouse);
                    }
                    Event::Paste(text) => {
                        self.paste_text(text);
                    }
                    Event::Resize(cols, rows) => {
                        self.selection = None;
                        self.width = cols;
                        self.height = rows;
                        self.resize_session();
                    }
                    _ => {}
                }
                if self.fatal.is_some()
                    || self.exit_message.is_some()
                    || input_started.elapsed() >= Duration::from_millis(8)
                {
                    break;
                }
            }

            if self.fatal.is_some() || self.exit_message.is_some() {
                break;
            }
            if self
                .status
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() > STATUS_LIFETIME)
            {
                self.status = None;
            }
            terminal
                .terminal()
                .draw(|frame| self.render(frame))
                .context("could not draw the FACT UI")?;
        }

        if let Some(error) = self.fatal.take() {
            return Err(error);
        }
        Ok(())
    }

    fn drain_events(&mut self) {
        for _ in 0..MAX_EVENTS_PER_TURN {
            match self.events.receiver.try_recv() {
                Ok(UiEvent::Message(message)) => self.handle_message(*message),
                Ok(UiEvent::Closed(reason)) => {
                    self.fatal = Some(anyhow!("daemon event stream closed: {reason}"));
                    return;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.fatal = Some(anyhow!("daemon event stream stopped unexpectedly"));
                    return;
                }
            }
        }
    }

    fn handle_message(&mut self, message: fact_protocol::ServerMessage) {
        match message {
            fact_protocol::ServerMessage::Frame(frame) => self.handle_frame(*frame),
            fact_protocol::ServerMessage::SessionEnded { reason } => {
                self.exit_message = Some(format!(
                    "Session '{}' ended: {reason}",
                    self.info.session.name
                ));
            }
            _ => {}
        }
    }

    fn handle_frame(&mut self, frame: Frame) {
        if frame.seq <= self.last_seq {
            return;
        }
        if self.detach_base.is_none() {
            self.detach_base = Some(frame.detach_seq);
        } else if frame.detach_seq > self.detach_base.unwrap_or(0) {
            self.exit_message = Some(format!(
                "Detached from '{}'. Its shells are still running; use `fact attach {}` to return.",
                frame.session.name, frame.session.name
            ));
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
        for pane in &frame.panes {
            if let Some(cells) = &pane.cells {
                let expected = usize::from(pane.content_cols) * usize::from(pane.content_rows);
                if expected == cells.len() {
                    self.buffers.insert(
                        pane.id,
                        PaneBuffer {
                            cols: pane.content_cols,
                            rows: pane.content_rows,
                            cells: cells.clone(),
                        },
                    );
                }
            }
            let previous = self.last_bells.insert(pane.id, pane.bell_seq).unwrap_or(0);
            if pane.bell_seq > previous && self.last_seq != 0 && !pane.focused {
                self.bells.insert(pane.id);
            }
        }

        self.info.session = frame.session.clone();
        self.last_seq = frame.seq;
        self.frame = Some(frame);
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
                self.set_status(format!(
                    "Scroll mode: {history} lines. Up/Down, PgUp/PgDn, Home/End, Esc exits."
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
                self.set_status(format!("daemon: {error}"));
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

        if let Some(action) = direct_grid_action(key) {
            self.do_action(action);
            return;
        }

        // Windows-terminal muscle memory: Ctrl+C copies a visible selection,
        // Ctrl+V pastes, and Ctrl/Shift+Insert are accepted as aliases.
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            CKeyCode::Esc if self.selection.is_some() => {
                self.selection = None;
                return;
            }
            CKeyCode::Char('v' | 'V') if control && !shift => {
                self.paste_clipboard();
                return;
            }
            CKeyCode::Char('c' | 'C') if control && !shift && self.selection.is_some() => {
                self.copy_selection();
                return;
            }
            CKeyCode::Insert if shift => {
                self.paste_clipboard();
                return;
            }
            CKeyCode::Insert if control => {
                self.copy_selection();
                return;
            }
            _ => {}
        }

        if key.code == CKeyCode::F(2) {
            self.theme = self.theme.next();
            return;
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
        let pane_id = pane.id;
        let Some(bytes) = keymap::encode_key(key, app_cursor) else {
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
                // When the window is too small the daemon falls back to
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
            "sessions" => self.open_sessions(),
            "help" => {
                self.overlay = Some(Overlay::Help(HelpState::default()));
            }
            "detach" => {
                self.exit_message = Some(format!(
                    "Detached from '{}'. Its shells are still running.",
                    self.info.session.name
                ));
            }
            "quit" => {
                self.exit_message = Some(format!(
                    "Closed the FACT UI for '{}'. Its shells are still running.",
                    self.info.session.name
                ));
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

    fn open_sessions(&mut self) {
        let Some(ResponsePayload::Sessions(sessions)) = self.send(Command::ListSessions {
            include_archived: false,
        }) else {
            return;
        };
        if sessions.is_empty() {
            self.set_status("No sessions exist yet.");
            return;
        }
        self.overlay = Some(Overlay::Sessions(SessionsState {
            sessions,
            selected: 0,
        }));
    }

    fn attach_session(&mut self, name: &str) {
        let Some(ResponsePayload::Attached(info)) = self.send(Command::Attach {
            name: name.to_string(),
        }) else {
            return;
        };
        let info = *info;
        let stream = match self.client.subscribe(&info.stream_token) {
            Ok(stream) => stream,
            Err(error) => {
                self.set_status(format!("Could not subscribe to '{name}': {error}"));
                return;
            }
        };
        let display_name = info.session.name.clone();
        self.stop_events();
        self.events = spawn_reader(stream);
        self.info = info;
        self.frame = None;
        self.buffers.clear();
        self.last_seq = 0;
        self.detach_base = None;
        self.last_bells.clear();
        self.bells.clear();
        self.selection = None;
        self.drag_split = None;
        self.scroll_drag = None;
        self.mouse_position = None;
        self.scroll_mode = None;
        self.app_mouse_down = AppMouseCapture::default();
        self.overlay = None;
        self.set_status(format!("Attached to '{display_name}'."));
        self.resize_session();
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
                match key.code {
                    CKeyCode::Esc | CKeyCode::F(1) => {}
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
                            if action == "__theme" {
                                self.theme = self.theme.next();
                                self.set_status(format!("Theme: {}", self.theme.name));
                            } else {
                                self.do_action(action);
                            }
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
                    CKeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
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
                            let _ = self.send(Command::FocusPane { pane });
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
                CKeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
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
                CKeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
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
            Overlay::Sessions(mut sessions) => match key.code {
                CKeyCode::Esc => {}
                CKeyCode::Up => {
                    sessions.selected = sessions.selected.saturating_sub(1);
                    self.overlay = Some(Overlay::Sessions(sessions));
                }
                CKeyCode::Down => {
                    sessions.selected =
                        (sessions.selected + 1).min(sessions.sessions.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Sessions(sessions));
                }
                CKeyCode::Enter => {
                    if let Some(session) = sessions.sessions.get(sessions.selected) {
                        let name = session.name.clone();
                        self.attach_session(&name);
                    }
                }
                _ => self.overlay = Some(Overlay::Sessions(sessions)),
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
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if let Some(drag) = self.scroll_drag.take() {
                if let Some(content_y) = self.content_row(mouse.row) {
                    self.scroll_drag_to(&drag, content_y);
                }
                return;
            }
            if self.drag_split.take().is_some() {
                return;
            }
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
                if let Some(geometry) = self.scrollbar_at(mouse.column, content_y) {
                    let _ = self.send(Command::FocusPane {
                        pane: geometry.pane,
                    });
                    self.bells.remove(&geometry.pane);
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
                    self.drag_split = Some(handle);
                    return;
                }
                let Some(pane) = self.pane_slot_at(mouse.column, content_y) else {
                    self.selection = None;
                    return;
                };
                let _ = self.send(Command::FocusPane { pane });
                self.bells.remove(&pane);
                let Some((_pane, local_x, local_y)) = self.pane_at(mouse.column, content_y) else {
                    self.selection = None;
                    return;
                };
                if let Some(view) = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                {
                    let app_mouse = app_tracks_mouse(view.modes)
                        && view.scroll_offset == 0
                        && !mouse.modifiers.contains(KeyModifiers::SHIFT);
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
                let Some(pane) = self.pane_slot_at(mouse.column, content_y) else {
                    return;
                };
                let _ = self.send(Command::FocusPane { pane });
                self.bells.remove(&pane);
                let app_mouse = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                    .is_some_and(|view| app_tracks_mouse(view.modes) && view.scroll_offset == 0)
                    && !mouse.modifiers.contains(KeyModifiers::SHIFT);
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
            MouseEventKind::Up(MouseButton::Left) => {
                let dragged = self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.anchor != selection.end);
                if dragged && self.config.copy_on_select {
                    self.copy_selection();
                } else if !dragged {
                    self.selection = None;
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
            Some(
                Overlay::Context(_) | Overlay::Help(_) | Overlay::Palette(_) | Overlay::Sessions(_)
            )
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

fn palette_items(keys: &CompiledKeybinds) -> Vec<PaletteItem> {
    let mut items: Vec<PaletteItem> = ACTION_INFO
        .iter()
        .map(|(action, label)| PaletteItem {
            action,
            label,
            shortcut: keys.describe(action),
        })
        .collect();
    items.push(PaletteItem {
        action: "__theme",
        label: "Cycle color theme",
        shortcut: "F2".into(),
    });
    items
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
    lines.push(("Cycle theme".into(), "F2".into()));
    lines.push(("Resize split".into(), "drag the gutter".into()));
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
        "Quick clipboard".into(),
        "Ctrl+C copy · Ctrl+V paste".into(),
    ));
    lines.push((
        "Select text".into(),
        "drag in a pane (Shift+drag if the app uses the mouse)".into(),
    ));
    lines.push(("Direct maximize / show all".into(), "Alt+M / Alt+N".into()));
    lines.push(("Terminate active grid".into(), "Alt+T".into()));
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

pub(crate) fn utc_clock(epoch_ms: i64) -> String {
    let total_seconds = epoch_ms.div_euclid(1000);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
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
    fn clock_formats_utc() {
        assert_eq!(utc_clock(0), "00:00:00Z");
        assert_eq!(utc_clock(3_661_000), "01:01:01Z");
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
