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
const STATUS_LIFETIME: Duration = Duration::from_secs(6);

/// Width of the new-tab button in the tab strip.
const NEW_TAB_WIDTH: u16 = 3;
/// Five two-column pane-header buttons (`V H _ ^ X`).
const PANE_BUTTON_WIDTH: u16 = 2;
const PANE_BUTTONS_WIDTH: u16 = PANE_BUTTON_WIDTH * 5;

pub(crate) const ACTION_INFO: &[(&str, &str)] = &[
    ("split_right", "Split right"),
    ("split_down", "Split down"),
    ("close_pane", "Close pane"),
    ("close_tab", "Close tab"),
    ("restart_pane", "Restart pane"),
    ("zoom", "Maximize / restore pane"),
    ("minimize", "Minimize pane"),
    ("restore", "Restore panes"),
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
    let (tx, rx) = mpsc::channel();
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
}

fn selection_text(buffers: &HashMap<u64, PaneBuffer>, selection: &Selection) -> Option<String> {
    let buffer = buffers.get(&selection.pane)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneChromeAction {
    SplitRight,
    SplitDown,
    Minimize,
    ToggleZoom,
    Close,
}

#[derive(Debug, Clone, Copy)]
struct PaneButton {
    action: PaneChromeAction,
    x: u16,
}

/// The header controls are right-aligned and share one render/hit-test layout.
/// Each button owns two columns: the glyph and the blank separator after it.
fn pane_buttons(width: u16) -> Vec<PaneButton> {
    if width < PANE_BUTTONS_WIDTH + 2 {
        return Vec::new();
    }
    let start = width - PANE_BUTTONS_WIDTH;
    let buttons = [
        PaneChromeAction::SplitRight,
        PaneChromeAction::SplitDown,
        PaneChromeAction::Minimize,
        PaneChromeAction::ToggleZoom,
        PaneChromeAction::Close,
    ];
    buttons
        .into_iter()
        .enumerate()
        .map(|(index, action)| PaneButton {
            action,
            x: start + index as u16 * PANE_BUTTON_WIDTH,
        })
        .collect()
}

fn pane_button_strip(zoomed: bool) -> String {
    format!("V H _ {} X ", if zoomed { 'v' } else { '^' })
}

fn pane_button_at(pane: &PaneView, column: u16, content_y: u16) -> Option<PaneChromeAction> {
    if content_y != pane.slot.y || !pane.slot.contains(column, content_y) {
        return None;
    }
    pane_buttons(pane.slot.width)
        .into_iter()
        .find(|button| {
            let x = pane.slot.x.saturating_add(button.x);
            column >= x && column < x.saturating_add(PANE_BUTTON_WIDTH)
        })
        .map(|button| button.action)
}

struct TabRange {
    id: u32,
    label: String,
    rect: RectDto,
}

fn tab_ranges(tabs: &[TabView], width: u16) -> Vec<TabRange> {
    let limit = width.saturating_sub(NEW_TAB_WIDTH + 1);
    let mut x = 0u16;
    let mut ranges = Vec::new();
    for tab in tabs {
        if x >= limit {
            break;
        }
        let available = limit.saturating_sub(x);
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

fn new_tab_rect(width: u16) -> Option<RectDto> {
    (width >= NEW_TAB_WIDTH).then(|| RectDto {
        x: width - NEW_TAB_WIDTH,
        y: 0,
        width: NEW_TAB_WIDTH,
        height: 1,
    })
}

fn minimized_tray_rect(width: u16, count: usize) -> Option<RectDto> {
    if count == 0 || width < 8 {
        return None;
    }
    let label = format!("▣ {count} ");
    let tray_width = label.chars().count() as u16;
    Some(RectDto {
        x: width.saturating_sub(tray_width),
        y: 0,
        width: tray_width,
        height: 1,
    })
}

#[derive(Debug, Clone)]
struct ScrollbarGeometry {
    pane: u64,
    track: RectDto,
    thumb_y: u16,
    thumb_height: u16,
    max_offset: usize,
}

fn scrollbar_geometry(pane: &PaneView) -> Option<ScrollbarGeometry> {
    let height = pane.slot.height.saturating_sub(1).min(pane.content_rows);
    // Only show the bar once scrollback is meaningful. One or two prompt lines
    // would otherwise render a near-full-height thumb that reads as a border.
    let meaningful = (usize::from(pane.content_rows) / 2).max(1);
    if pane.history_lines < meaningful || height == 0 || pane.slot.width < 2 {
        return None;
    }
    let total = pane
        .history_lines
        .saturating_add(usize::from(pane.content_rows));
    if total == 0 {
        return None;
    }
    let track_height = height;
    let thumb_height = ((usize::from(track_height) * usize::from(track_height) / total)
        .max(1)
        .min(usize::from(track_height))) as u16;
    let span = track_height.saturating_sub(thumb_height);
    let offset = pane.scroll_offset.min(pane.history_lines);
    let thumb_top =
        span.saturating_sub(((u64::from(span) * offset as u64) / pane.history_lines as u64) as u16);
    Some(ScrollbarGeometry {
        pane: pane.id,
        track: RectDto {
            x: pane
                .slot
                .x
                .saturating_add(pane.slot.width)
                .saturating_sub(1),
            y: pane.slot.y.saturating_add(1),
            width: 1,
            height: track_height,
        },
        thumb_y: pane.slot.y.saturating_add(1).saturating_add(thumb_top),
        thumb_height,
        max_offset: pane.history_lines,
    })
}

fn scrollbar_offset_at(geometry: &ScrollbarGeometry, content_y: u16) -> usize {
    if geometry.max_offset == 0 || geometry.track.height <= 1 {
        return geometry.max_offset;
    }
    let relative = content_y
        .saturating_sub(geometry.track.y)
        .min(geometry.track.height.saturating_sub(1));
    let span = geometry.track.height.saturating_sub(1);
    let ratio = f64::from(relative) / f64::from(span);
    ((1.0 - ratio) * geometry.max_offset as f64).round() as usize
}

#[derive(Debug, Clone)]
struct ScrollDrag {
    geometry: ScrollbarGeometry,
    last_sent: Instant,
}

#[derive(Debug)]
enum Overlay {
    Cold(ColdState),
    Help(HelpState),
    Palette(PaletteState),
    Search(SearchState),
    Prompt(PromptState),
    Confirm(ConfirmState),
    Context(ContextState),
    Sessions(SessionsState),
}

#[derive(Debug)]
struct ColdState {
    notes: Vec<String>,
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
    app_mouse_down: Option<(MouseButton, u64)>,
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
        let overlay = cold_overlay(&info);
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
            overlay,
            prefix_armed: false,
            selection: None,
            drag_split: None,
            scroll_drag: None,
            app_mouse_down: None,
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

            if event::poll(FRAME_POLL).context("could not poll terminal input")? {
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
                        self.width = cols;
                        self.height = rows;
                        self.resize_session();
                    }
                    _ => {}
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
        loop {
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
                self.set_status(format!("Bell in pane {}", pane.title));
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

    fn active_tab(&self) -> Option<&TabView> {
        self.frame.as_ref()?.tabs.iter().find(|tab| tab.active)
    }

    fn active_tab_id(&self) -> Option<u32> {
        self.active_tab().map(|tab| tab.id)
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

        // Windows-terminal muscle memory: Ctrl+C copies a visible selection,
        // Ctrl+V pastes, and Ctrl/Shift+Insert are accepted as aliases.
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
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
        if text.is_empty() {
            return;
        }
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
                        format!("Close tab '{}' and all of its panes?", tab.title),
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
                if let Some(tab) = self.active_tab() {
                    let zoomed = !tab.zoomed;
                    let _ = self.send(Command::ZoomPane { zoomed });
                }
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

    fn do_pane_action(&mut self, pane: u64, action: PaneChromeAction) {
        self.bells.clear();
        match action {
            PaneChromeAction::SplitRight => {
                let _ = self.send(Command::SplitPane {
                    pane,
                    direction: SplitDirection::Right,
                });
            }
            PaneChromeAction::SplitDown => {
                let _ = self.send(Command::SplitPane {
                    pane,
                    direction: SplitDirection::Down,
                });
            }
            PaneChromeAction::Minimize => {
                if self.send(Command::MinimizePane { pane }).is_some() {
                    let restore = self
                        .keys
                        .binding("restore")
                        .map(|binding| binding.to_string())
                        .unwrap_or_else(|| "prefix+Shift+M".into());
                    self.set_status(format!(
                        "Pane minimized. Click the tray or press {restore} to restore."
                    ));
                }
            }
            PaneChromeAction::ToggleZoom => {
                if let Some(tab) = self.active_tab() {
                    let _ = self.send(Command::ZoomPane {
                        zoomed: !tab.zoomed,
                    });
                }
            }
            PaneChromeAction::Close => {
                self.confirm_or_send(
                    format!(
                        "Close pane '{}'? Its process will be stopped.",
                        self.pane_label(pane)
                    ),
                    Command::ClosePane { pane },
                );
            }
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
        let cold = info.cold_recovered.then(|| ColdState {
            notes: info.recovery_notes.clone(),
        });
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
        self.app_mouse_down = None;
        self.overlay = cold.map(Overlay::Cold);
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
            Overlay::Cold(cold) => {
                if matches!(key.code, CKeyCode::Enter | CKeyCode::Esc)
                    || matches!(key.code, CKeyCode::Char(' '))
                {
                    self.set_status("Continuing in the recovered session.");
                } else {
                    self.overlay = Some(Overlay::Cold(cold));
                }
            }
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
        if self.overlay.is_some() {
            return;
        }
        if mouse.row == 0 {
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
        if mouse.row + 1 >= self.height {
            let count = self
                .frame
                .as_ref()
                .map(|frame| frame.minimized.len())
                .unwrap_or(0);
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && minimized_tray_rect(self.width, count)
                    .is_some_and(|rect| rect.contains(mouse.column, 0))
            {
                let _ = self.send(Command::RestorePanes);
            }
            return;
        }

        let content_y = mouse.row - 1;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((pane, action)) = self.pane_header_at(mouse.column, content_y) {
                    let _ = self.send(Command::FocusPane { pane });
                    self.bells.remove(&pane);
                    self.do_pane_action(pane, action);
                    return;
                }
                if self
                    .pane_slot_at(mouse.column, content_y)
                    .is_some_and(|pane| {
                        self.frame
                            .as_ref()
                            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                            .is_some_and(|view| view.slot.y == content_y)
                    })
                {
                    if let Some(pane) = self.pane_slot_at(mouse.column, content_y) {
                        let _ = self.send(Command::FocusPane { pane });
                        self.bells.remove(&pane);
                    }
                    return;
                }
                if let Some(geometry) = self.scrollbar_at(mouse.column, content_y) {
                    let _ = self.send(Command::FocusPane {
                        pane: geometry.pane,
                    });
                    self.bells.remove(&geometry.pane);
                    self.scroll_to(&geometry, content_y);
                    self.scroll_drag = Some(ScrollDrag {
                        geometry,
                        last_sent: Instant::now() - Duration::from_millis(100),
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
                    let app_mouse = view.modes & pane_modes::MOUSE_CLICK != 0
                        && !mouse.modifiers.contains(KeyModifiers::SHIFT);
                    if app_mouse {
                        self.app_mouse_down = Some((MouseButton::Left, pane));
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
                    .is_some_and(|view| view.modes & pane_modes::MOUSE_CLICK != 0);
                if app_mouse {
                    if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                        self.forward_mouse_button(
                            MouseButton::Right,
                            pane,
                            x,
                            y,
                            true,
                            mouse.modifiers,
                        );
                        self.forward_mouse_button(
                            MouseButton::Right,
                            pane,
                            x,
                            y,
                            false,
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
                            "zoom",
                            "minimize",
                            "rename_tab",
                            "search",
                            "restart_pane",
                            "close_pane",
                            "copy",
                        ],
                    }));
                }
            }
            MouseEventKind::Drag(button) => {
                if button == MouseButton::Left {
                    if let Some(drag) = self.scroll_drag.clone() {
                        if drag.last_sent.elapsed() >= Duration::from_millis(35) {
                            self.scroll_to(&drag.geometry, content_y);
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
                if let Some((held, pane)) = self.app_mouse_down {
                    let drags = held == button
                        && button == MouseButton::Left
                        && self
                            .frame
                            .as_ref()
                            .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                            .is_some_and(|view| view.modes & pane_modes::MOUSE_DRAG != 0);
                    if drags {
                        if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                            let data = keymap::sgr_mouse_motion(button, x, y, mouse.modifiers);
                            let _ = self.send(Command::Input { pane, data });
                        }
                    }
                    return;
                }
                if button == MouseButton::Left {
                    if let Some((pane, x, y)) = self.pane_at(mouse.column, content_y) {
                        if let Some(selection) = &mut self.selection {
                            if selection.pane == pane {
                                selection.end = (x, y);
                            }
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(drag) = self.scroll_drag.take() {
                    self.scroll_to(&drag.geometry, content_y);
                    return;
                }
                if self.drag_split.take().is_some() {
                    return;
                }
                if let Some((button, pane)) = self.app_mouse_down.take() {
                    if let Some((x, y)) = self.pane_local(pane, mouse.column, content_y) {
                        self.forward_mouse_button(button, pane, x, y, false, mouse.modifiers);
                    }
                    return;
                }
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
                if let Some(geometry) = self.scrollbar_at(mouse.column, content_y) {
                    let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        3
                    } else {
                        -3
                    };
                    let _ = self.send(Command::ScrollPane {
                        pane: geometry.pane,
                        delta,
                    });
                    return;
                }
                let Some((pane, x, y)) = self.pane_at(mouse.column, content_y) else {
                    return;
                };
                let app_mouse = self
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.panes.iter().find(|view| view.id == pane))
                    .is_some_and(|view| view.modes & pane_modes::MOUSE_CLICK != 0);
                if app_mouse {
                    let data = keymap::sgr_wheel(
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                        x,
                        y,
                        mouse.modifiers,
                    );
                    let _ = self.send(Command::Input { pane, data });
                } else {
                    let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        3
                    } else {
                        -3
                    };
                    let _ = self.send(Command::ScrollPane { pane, delta });
                }
            }
            _ => {}
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
        let data = keymap::sgr_mouse(button, x, y, pressed, modifiers);
        let _ = self.send(Command::Input { pane, data });
    }

    fn pane_header_at(&self, column: u16, content_y: u16) -> Option<(u64, PaneChromeAction)> {
        let frame = self.frame.as_ref()?;
        frame.panes.iter().find_map(|pane| {
            pane_button_at(pane, column, content_y).map(|action| (pane.id, action))
        })
    }

    fn scrollbar_at(&self, column: u16, content_y: u16) -> Option<ScrollbarGeometry> {
        let frame = self.frame.as_ref()?;
        frame.panes.iter().find_map(|pane| {
            scrollbar_geometry(pane).filter(|geometry| geometry.track.contains(column, content_y))
        })
    }

    fn scroll_to(&mut self, geometry: &ScrollbarGeometry, content_y: u16) {
        let offset = scrollbar_offset_at(geometry, content_y);
        let _ = self.send(Command::SetScrollOffset {
            pane: geometry.pane,
            offset,
        });
    }

    fn pane_at(&self, column: u16, content_y: u16) -> Option<(u64, u16, u16)> {
        let frame = self.frame.as_ref()?;
        for pane in &frame.panes {
            if pane.slot.contains(column, content_y) {
                let content_x = pane.slot.x;
                let content_y0 = pane.slot.y + 1;
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
        let origin_y = view.slot.y + 1;
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

fn cold_overlay(info: &AttachInfo) -> Option<Overlay> {
    if info.cold_recovered {
        let mut notes = info.recovery_notes.clone();
        if notes.is_empty() {
            notes.push("The previous shell processes are gone; new shells were started.".into());
        }
        notes.push(
            "Saved output was restored for context. Commands are never replayed automatically."
                .into(),
        );
        Some(Overlay::Cold(ColdState { notes }))
    } else {
        None
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

pub(crate) fn help_lines(keys: &CompiledKeybinds) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = ACTION_INFO
        .iter()
        .map(|(action, label)| (label.to_string(), keys.describe(action)))
        .collect();
    lines.push(("Cycle theme".into(), "F2".into()));
    lines.push(("Resize split".into(), "drag the gutter".into()));
    lines.push((
        "Pane buttons".into(),
        "V split right · H split down · _ min · ^ max · X close".into(),
    ));
    lines.push(("New tab".into(), "click + in the tab bar".into()));
    lines.push(("Scrollback".into(), "mouse wheel or drag the bar".into()));
    lines.push(("Bell".into(), "the pane title turns amber".into()));
    lines.push((
        "Quick clipboard".into(),
        "Ctrl+C copy · Ctrl+V paste".into(),
    ));
    lines.push((
        "Select text".into(),
        "drag in a pane (Shift+drag if the app uses the mouse)".into(),
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
        };
        assert_eq!(selection_text(&buffers, &selection).unwrap(), "界 !");
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
            content_cols: width.saturating_sub(1),
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
            cold_restored: false,
            focused: true,
        }
    }

    #[test]
    fn pane_header_buttons_have_stable_hit_targets() {
        let view = test_view(24);
        assert_eq!(pane_buttons(24).len(), 5);
        assert!(pane_buttons(11).is_empty());
        assert_eq!(
            pane_button_at(&view, 24, 2),
            Some(PaneChromeAction::SplitRight)
        );
        assert_eq!(
            pane_button_at(&view, 26, 2),
            Some(PaneChromeAction::SplitDown)
        );
        assert_eq!(
            pane_button_at(&view, 28, 2),
            Some(PaneChromeAction::Minimize)
        );
        assert_eq!(
            pane_button_at(&view, 30, 2),
            Some(PaneChromeAction::ToggleZoom)
        );
        assert_eq!(pane_button_at(&view, 32, 2), Some(PaneChromeAction::Close));
        assert_eq!(pane_button_at(&view, 24, 3), None);
        assert_eq!(pane_button_at(&view, 34, 2), None);
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
    fn minimized_tray_is_right_aligned() {
        let tray = minimized_tray_rect(10, 2).unwrap();
        assert_eq!(tray.x + tray.width, 10);
        assert!(tray.contains(6, 0));
        assert!(tray.contains(9, 0));
        assert!(!tray.contains(5, 0));
    }

    #[test]
    fn scrollbar_maps_track_position_to_scrollback_offset() {
        let mut view = test_view(40);
        view.history_lines = 2;
        view.content_rows = 10;
        assert!(scrollbar_geometry(&view).is_none());
        view.history_lines = 100;
        view.content_rows = 10;
        view.slot.height = 11;
        let bottom = scrollbar_geometry(&view).unwrap();
        assert_eq!(bottom.track.x, 49);
        assert_eq!(bottom.track.y, 3);
        assert_eq!(bottom.track.height, 10);
        assert_eq!(bottom.max_offset, 100);
        assert_eq!(scrollbar_offset_at(&bottom, bottom.track.y), 100);
        assert_eq!(
            scrollbar_offset_at(&bottom, bottom.track.y + bottom.track.height - 1),
            0
        );
        view.scroll_offset = 100;
        let top = scrollbar_geometry(&view).unwrap();
        assert_eq!(top.thumb_y, top.track.y);
    }
}
