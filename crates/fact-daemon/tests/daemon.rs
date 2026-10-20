//! End-to-end daemon tests against real ConPTY shells.
//!
//! Each test uses a unique temporary FACT home, so pipe names, databases and
//! daemon identities are isolated even when tests run in parallel.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use fact_config::{Config, Paths};
use fact_protocol::{
    AttachInfo, ClientMessage, Command, ErrorCode, Frame, Hello, ResponsePayload, ServerMessage,
    SplitDirection,
};
use fact_runtime::{connect, DaemonClient, DaemonIdentity, RuntimeError};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("create temp directory");
    let base = dir.path().join("fact-home");
    let paths = Paths::from_parts(base.clone(), base.join("config.toml"));
    Harness { _dir: dir, paths }
}

fn start_daemon_with(harness: &Harness, config: Config) -> thread::JoinHandle<anyhow::Result<()>> {
    let paths = harness.paths.clone();
    thread::Builder::new()
        .name("fact-daemon-test".into())
        .spawn(move || fact_daemon::run_with_config(paths, config))
        .expect("spawn daemon thread")
}

fn start_daemon(harness: &Harness) -> thread::JoinHandle<anyhow::Result<()>> {
    start_daemon_with(harness, Config::default())
}

fn wait_for_daemon(harness: &Harness) -> (DaemonIdentity, DaemonClient) {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match connect(&harness.paths) {
            Ok(connected) => return connected,
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    panic!(
        "daemon did not start: {:?}\n--- daemon log ---\n{}",
        last_error,
        read_log(harness)
    );
}

fn read_log(harness: &Harness) -> String {
    std::fs::read_to_string(&harness.paths.daemon_log).unwrap_or_else(|_| "<no log>".into())
}

fn resume(client: &mut DaemonClient, name: &str, grid: u8) -> AttachInfo {
    match client
        .request(Command::ResumeSession {
            name: Some(name.to_string()),
            grid: Some(grid),
            profile: None,
            cwd: None,
        })
        .expect("resume session")
    {
        ResponsePayload::Attached(info) => *info,
        other => panic!("expected Attached response, got {other:?}"),
    }
}

fn resume_existing(client: &mut DaemonClient, name: &str) -> AttachInfo {
    match client
        .request(Command::ResumeSession {
            name: Some(name.to_string()),
            grid: None,
            profile: None,
            cwd: None,
        })
        .expect("resume existing session")
    {
        ResponsePayload::Attached(info) => *info,
        other => panic!("expected Attached response, got {other:?}"),
    }
}

fn attach(client: &mut DaemonClient, name: &str) -> AttachInfo {
    match client
        .request(Command::Attach {
            name: name.to_string(),
        })
        .expect("attach session")
    {
        ResponsePayload::Attached(info) => *info,
        other => panic!("expected Attached response, got {other:?}"),
    }
}

fn event_reader(client: &DaemonClient, token: &str) -> Receiver<ServerMessage> {
    let stream = client.subscribe(token).expect("subscribe to events");
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("fact-test-events".into())
        .spawn(move || {
            let mut stream = stream;
            loop {
                match stream.recv() {
                    Ok(message) => {
                        if tx.send(message).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        })
        .expect("spawn event reader");
    rx
}

fn recv_frame(rx: &Receiver<ServerMessage>, timeout: Duration) -> Option<Frame> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match rx.recv_timeout(remaining) {
            Ok(ServerMessage::Frame(frame)) => return Some(*frame),
            Ok(ServerMessage::SessionEnded { reason }) => {
                panic!("session ended while waiting for a frame: {reason}")
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn wait_for_frame(
    rx: &Receiver<ServerMessage>,
    timeout: Duration,
    predicate: impl Fn(&Frame) -> bool,
) -> Option<Frame> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = recv_frame(rx, remaining)?;
        if predicate(&frame) {
            return Some(frame);
        }
    }
}

fn frame_text(frame: &Frame, buffers: &mut HashMap<u64, String>) -> String {
    for pane in &frame.panes {
        if let Some(cells) = &pane.cells {
            let cols = pane.content_cols.max(1) as usize;
            let mut text = String::with_capacity(cells.len() * 2);
            for (index, cell) in cells.iter().enumerate() {
                if index > 0 && index % cols == 0 {
                    text.push('\n');
                }
                text.push_str(&cell.symbol());
            }
            buffers.insert(pane.id, text);
        }
    }
    buffers.values().cloned().collect::<Vec<_>>().join("\n")
}

fn wait_for_text(
    rx: &Receiver<ServerMessage>,
    buffers: &mut HashMap<u64, String>,
    needle: &str,
    timeout: Duration,
) -> Option<(Frame, String)> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = recv_frame(rx, remaining)?;
        let text = frame_text(&frame, buffers);
        if text.contains(needle) {
            return Some((frame, text));
        }
    }
}

fn shutdown(
    mut client: DaemonClient,
    handle: thread::JoinHandle<anyhow::Result<()>>,
    harness: &Harness,
) {
    match client.request(Command::ShutdownDaemon) {
        Ok(ResponsePayload::Ack) => {}
        Ok(other) => panic!("unexpected shutdown response: {other:?}"),
        Err(error) => panic!("shutdown request failed: {error}\n{}", read_log(harness)),
    }
    drop(client);
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("daemon failed: {error}\n{}", read_log(harness)),
        Err(_) => panic!("daemon thread panicked\n{}", read_log(harness)),
    }
    assert!(
        !harness.paths.daemon_json.exists(),
        "daemon identity was not removed on clean shutdown"
    );
}

#[test]
fn grid_launches_independent_shells_and_streams_output() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (identity, mut client) = wait_for_daemon(&harness);

    assert_eq!(identity.pid, client.daemon_pid());
    assert_eq!(identity.token.len(), 64);
    assert!(identity.pipe.starts_with(r"\\.\pipe\fact-"));

    match client.request(Command::Ping).expect("ping") {
        ResponsePayload::Pong { live_sessions, .. } => assert_eq!(live_sessions, 0),
        other => panic!("unexpected ping response: {other:?}"),
    }

    let info = resume(&mut client, "grid", 8);
    assert!(info.session.live);
    assert_eq!(info.session.pane_count, 8);
    assert_eq!(info.session.tab_count, 1);

    let events = event_reader(&client, &info.stream_token);
    let first = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 8 && frame.panes.iter().all(|pane| pane.cells.is_some())
    })
    .unwrap_or_else(|| panic!("no initial frame\n{}", read_log(&harness)));

    assert_eq!(first.session.name, "grid");
    assert!(first.session.live);
    assert!(first.focus.is_some());
    let mut pids: Vec<u32> = first.panes.iter().map(|pane| pane.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    assert_eq!(pids.len(), 8, "each grid cell must own a distinct process");
    assert!(first.panes.iter().all(|pane| pane.alive));

    let target = first.panes[0].id;
    client
        .request(Command::Input {
            pane: target,
            data: b"echo FACT_MARKER_ONE\r".to_vec(),
        })
        .expect("send input");

    let mut buffers = HashMap::new();
    let (_frame, text) = wait_for_text(&events, &mut buffers, "FACT_MARKER_ONE", FRAME_TIMEOUT)
        .unwrap_or_else(|| panic!("shell output never arrived\n{}", read_log(&harness)));
    assert!(text.contains("FACT_MARKER_ONE"));

    shutdown(client, handle, &harness);
}

#[test]
fn resume_rejects_a_grid_shape_that_changed_before_attach() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);

    let info = resume(&mut client, "one", 1);
    assert_eq!(info.session.pane_count, 1);

    let error = client
        .request(Command::ResumeSession {
            name: Some("one".to_string()),
            grid: Some(8),
            profile: None,
            cwd: None,
        })
        .expect_err("a one-pane session must not satisfy --grid 8");
    match error {
        RuntimeError::Request(error) => {
            assert_eq!(error.code, ErrorCode::Conflict);
            assert!(
                error.message.contains("1 pane(s)"),
                "unexpected error: {}",
                error.message
            );
        }
        other => panic!("unexpected resume error: {other:?}"),
    }

    let info = attach(&mut client, "one");
    assert_eq!(info.session.pane_count, 1);
    shutdown(client, handle, &harness);
}

#[test]
fn detach_and_reattach_keep_the_same_processes() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);

    let info = resume(&mut client, "detach", 1);
    let events = event_reader(&client, &info.stream_token);
    let first = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 1 && frame.panes[0].cells.is_some()
    })
    .expect("initial frame");
    let original_pid = first.panes[0].pid;
    let original_pane = first.panes[0].id;
    assert!(original_pid > 0);

    assert!(matches!(
        client
            .request(Command::DetachClients { name: None })
            .expect("detach"),
        ResponsePayload::Ack
    ));
    let detached =
        wait_for_frame(&events, FRAME_TIMEOUT, |frame| frame.detach_seq > 0).expect("detach frame");
    assert_eq!(detached.panes[0].pid, original_pid);
    drop(events);

    let reattached = attach(&mut client, "detach");
    assert_eq!(reattached.session.name, "detach");
    let events = event_reader(&client, &reattached.stream_token);
    let frame = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 1 && frame.panes[0].cells.is_some()
    })
    .expect("reattached frame");
    assert_eq!(frame.panes[0].id, original_pane);
    assert_eq!(
        frame.panes[0].pid, original_pid,
        "live reattach must keep the original shell process"
    );
    assert!(frame.panes[0].alive);

    shutdown(client, handle, &harness);
}

#[test]
fn saved_layout_starts_fresh_shells_without_replaying_output() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);

    let info = resume(&mut client, "restart", 2);
    let events = event_reader(&client, &info.stream_token);
    let initial = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 2 && frame.panes.iter().all(|pane| pane.cells.is_some())
    })
    .expect("initial frame");
    let old_pids: Vec<u32> = initial.panes.iter().map(|pane| pane.pid).collect();
    let first_pane = initial.panes[0].id;

    client
        .request(Command::Input {
            pane: first_pane,
            data: b"echo FACT_OLD_MARKER\r".to_vec(),
        })
        .expect("send input");
    let mut buffers = HashMap::new();
    wait_for_text(&events, &mut buffers, "FACT_OLD_MARKER", FRAME_TIMEOUT).expect("marker output");
    drop(events);

    assert!(matches!(
        client
            .request(Command::SaveSession {
                name: "restart".into()
            })
            .expect("save session"),
        ResponsePayload::Ack
    ));
    shutdown(client, handle, &harness);

    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);
    let restored = resume(&mut client, "restart", 2);
    assert!(restored.session.live);

    let events = event_reader(&client, &restored.stream_token);
    let frame = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 2 && frame.panes.iter().all(|pane| pane.cells.is_some())
    })
    .expect("restored layout frame");
    for pane in &frame.panes {
        assert!(
            !old_pids.contains(&pane.pid),
            "a saved layout must start fresh processes, not reuse old pids"
        );
    }

    let pane = frame.panes[0].id;
    let search = match client
        .request(Command::SearchPane {
            pane,
            query: "FACT_OLD_MARKER".into(),
            case_sensitive: true,
            limit: 20,
        })
        .expect("search fresh pane")
    {
        ResponsePayload::Search(result) => result,
        other => panic!("unexpected search response: {other:?}"),
    };
    assert!(
        search.hits.is_empty(),
        "old screen output must not be replayed after a daemon restart: {:?}",
        search.hits
    );

    client
        .request(Command::Input {
            pane,
            data: b"echo FACT_NEW_MARKER\r".to_vec(),
        })
        .expect("send input to fresh shell");
    wait_for_text(&events, &mut buffers, "FACT_NEW_MARKER", FRAME_TIMEOUT)
        .expect("fresh shell should accept input");

    shutdown(client, handle, &harness);
}

#[test]
fn split_tabs_focus_and_search_drive_real_panes() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);

    let info = resume(&mut client, "layout", 1);
    let events = event_reader(&client, &info.stream_token);
    let first = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 1 && frame.panes[0].cells.is_some()
    })
    .expect("initial frame");
    assert_eq!(
        first.viewport.slot_rows, 35,
        "a one-tab session must use the full viewport height"
    );
    let original_pane = first.panes[0].id;
    let original_tab = first.active_tab;

    client
        .request(Command::Input {
            pane: original_pane,
            data: b"echo FACT_LAYOUT_MARKER\r".to_vec(),
        })
        .expect("send marker");
    let mut buffers = HashMap::new();
    wait_for_text(&events, &mut buffers, "FACT_LAYOUT_MARKER", FRAME_TIMEOUT)
        .expect("marker output");

    assert!(matches!(
        client
            .request(Command::SplitPane {
                pane: original_pane,
                direction: SplitDirection::Right,
            })
            .expect("split pane"),
        ResponsePayload::Ack
    ));
    let split = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 2 && frame.splits.len() == 1
    })
    .expect("split frame");
    assert_eq!(split.session.pane_count, 2);
    let split_pane = split
        .panes
        .iter()
        .find(|pane| pane.id != original_pane)
        .map(|pane| pane.id)
        .expect("new pane");
    assert_ne!(split.panes[0].pid, split.panes[1].pid);

    assert!(matches!(
        client
            .request(Command::FocusPane { pane: split_pane })
            .expect("focus pane"),
        ResponsePayload::Ack
    ));
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.focus == Some(split_pane)
    })
    .expect("focus frame");

    assert!(matches!(
        client
            .request(Command::NewTab {
                profile: None,
                cwd: None,
            })
            .expect("new tab"),
        ResponsePayload::Ack
    ));
    let two_tabs = wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.tabs.len() == 2 && frame.session.pane_count == 3
    })
    .expect("new tab frame");
    assert_eq!(
        two_tabs.viewport.slot_rows, 34,
        "the tab strip must reserve exactly one row"
    );
    let new_tab = two_tabs.active_tab;
    let active_tab = two_tabs.active_tab;

    assert!(matches!(
        client
            .request(Command::RenameTab {
                tab: active_tab,
                title: "build".into(),
            })
            .expect("rename tab"),
        ResponsePayload::Ack
    ));
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame
            .tabs
            .iter()
            .any(|tab| tab.id == active_tab && tab.title == "build")
    })
    .expect("renamed tab frame");

    assert!(matches!(
        client
            .request(Command::SelectTab { tab: original_tab })
            .expect("select tab"),
        ResponsePayload::Ack
    ));
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.active_tab == original_tab
    })
    .expect("selected tab frame");

    let search = match client
        .request(Command::SearchPane {
            pane: original_pane,
            query: "FACT_LAYOUT_MARKER".into(),
            case_sensitive: true,
            limit: 20,
        })
        .expect("search")
    {
        ResponsePayload::Search(result) => result,
        other => panic!("unexpected search response: {other:?}"),
    };
    assert_eq!(search.pane, original_pane);
    assert!(!search.hits.is_empty());
    assert!(search
        .hits
        .iter()
        .any(|hit| hit.preview.contains("FACT_LAYOUT_MARKER")));

    // Fill the scrollback without relying on a specific shell language, then
    // exercise the direct offset command used by scrollbar dragging.
    client
        .request(Command::Input {
            pane: original_pane,
            data: b"\r\n".repeat(48),
        })
        .expect("fill scrollback");
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame
            .panes
            .iter()
            .any(|pane| pane.id == original_pane && pane.history_lines >= 8)
    })
    .expect("scrollback frame");
    // Let the shell finish draining the queued blank commands; new output
    // returns the viewport to the live bottom.
    thread::sleep(Duration::from_millis(750));
    client
        .request(Command::SetScrollOffset {
            pane: original_pane,
            offset: 5,
        })
        .expect("set scroll offset");
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame
            .panes
            .iter()
            .any(|pane| pane.id == original_pane && pane.scroll_offset == 5)
    })
    .expect("scrolled frame");
    client
        .request(Command::SetScrollOffset {
            pane: original_pane,
            offset: 0,
        })
        .expect("reset scroll offset");
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame
            .panes
            .iter()
            .any(|pane| pane.id == original_pane && pane.scroll_offset == 0)
    })
    .expect("live scroll frame");

    assert!(matches!(
        client
            .request(Command::CloseTab { tab: new_tab })
            .expect("close tab"),
        ResponsePayload::Ack
    ));
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.tabs.len() == 1 && frame.session.pane_count == 2
    })
    .expect("closed tab frame");

    shutdown(client, handle, &harness);
}

#[test]
fn session_lifecycle_rename_archive_export_import_and_delete() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (_identity, mut client) = wait_for_daemon(&harness);

    let info = resume(&mut client, "alpha", 1);
    let events = event_reader(&client, &info.stream_token);
    wait_for_frame(&events, FRAME_TIMEOUT, |frame| {
        frame.panes.len() == 1 && frame.panes[0].cells.is_some()
    })
    .expect("initial frame");

    let renamed = match client
        .request(Command::RenameSession {
            name: "alpha".into(),
            new_name: "beta".into(),
        })
        .expect("rename live session")
    {
        ResponsePayload::Session(summary) => summary,
        other => panic!("unexpected rename response: {other:?}"),
    };
    assert_eq!(renamed.name, "beta");
    assert!(renamed.live);

    // The control connection is bound to the session and must follow renames.
    assert!(matches!(
        client
            .request(Command::NewTab {
                profile: None,
                cwd: None,
            })
            .expect("new tab after rename"),
        ResponsePayload::Ack
    ));

    let export = match client
        .request(Command::ExportSession {
            name: "beta".into(),
        })
        .expect("export")
    {
        ResponsePayload::Export { filename, json } => {
            assert!(filename.starts_with("fact-beta-"));
            assert!(json.starts_with('{'));
            assert!(json.contains("fact-session"));
            json
        }
        other => panic!("unexpected export response: {other:?}"),
    };

    assert!(matches!(
        client
            .request(Command::TerminateSession {
                name: "beta".into()
            })
            .expect("terminate"),
        ResponsePayload::Ack
    ));
    drop(events);

    let listed = match client
        .request(Command::ListSessions {
            include_archived: false,
        })
        .expect("list")
    {
        ResponsePayload::Sessions(sessions) => sessions,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert!(listed
        .iter()
        .any(|session| session.name == "beta" && !session.live));

    let renamed = match client
        .request(Command::RenameSession {
            name: "beta".into(),
            new_name: "BETA".into(),
        })
        .expect("case-only rename")
    {
        ResponsePayload::Session(summary) => summary,
        other => panic!("unexpected rename response: {other:?}"),
    };
    assert_eq!(renamed.name, "BETA");

    let archived = match client
        .request(Command::ArchiveSession {
            name: "BETA".into(),
        })
        .expect("archive")
    {
        ResponsePayload::Session(summary) => summary,
        other => panic!("unexpected archive response: {other:?}"),
    };
    assert!(archived.archived);

    let visible = match client
        .request(Command::ListSessions {
            include_archived: false,
        })
        .expect("list visible")
    {
        ResponsePayload::Sessions(sessions) => sessions,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert!(visible.is_empty());

    let unarchived = match client
        .request(Command::UnarchiveSession {
            name: "BETA".into(),
        })
        .expect("unarchive")
    {
        ResponsePayload::Session(summary) => summary,
        other => panic!("unexpected unarchive response: {other:?}"),
    };
    assert!(!unarchived.archived);

    let recovered = resume_existing(&mut client, "BETA");
    assert!(recovered.session.live);

    let imported = match client
        .request(Command::ImportSession {
            json: export,
            name: Some("copy".into()),
            overwrite: false,
        })
        .expect("import")
    {
        ResponsePayload::Imported(summary) => summary,
        other => panic!("unexpected import response: {other:?}"),
    };
    assert_eq!(imported.name, "copy");
    assert!(!imported.live);

    assert!(matches!(
        client
            .request(Command::DeleteSession {
                name: "copy".into(),
                force: false,
            })
            .expect("delete copy"),
        ResponsePayload::Ack
    ));

    assert!(matches!(
        client
            .request(Command::TerminateSession {
                name: "BETA".into()
            })
            .expect("terminate recovered"),
        ResponsePayload::Ack
    ));
    assert!(matches!(
        client
            .request(Command::DeleteSession {
                name: "BETA".into(),
                force: false,
            })
            .expect("delete recovered"),
        ResponsePayload::Ack
    ));

    let remaining = match client
        .request(Command::ListSessions {
            include_archived: true,
        })
        .expect("list final")
    {
        ResponsePayload::Sessions(sessions) => sessions,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert!(remaining.is_empty());

    shutdown(client, handle, &harness);
}

#[test]
fn resuming_a_saved_session_respects_max_sessions() {
    let harness = harness();
    let config = Config {
        max_sessions: 1,
        ..Config::default()
    };
    let handle = start_daemon_with(&harness, config);
    let (_identity, mut client) = wait_for_daemon(&harness);

    resume(&mut client, "saved-cap", 1);
    assert!(matches!(
        client
            .request(Command::TerminateSession {
                name: "saved-cap".into(),
            })
            .expect("terminate saved-cap"),
        ResponsePayload::Ack
    ));

    resume(&mut client, "live-cap", 1);
    let error = client
        .request(Command::ResumeSession {
            name: Some("saved-cap".into()),
            grid: None,
            profile: None,
            cwd: None,
        })
        .expect_err("resuming a second saved session must respect max_sessions");
    match error {
        RuntimeError::Request(error) => {
            assert_eq!(error.code, ErrorCode::Busy);
            assert!(error.message.contains("maximum live session"));
        }
        other => panic!("expected a request error, got {other:?}"),
    }

    shutdown(client, handle, &harness);
}

#[test]
fn wrong_token_is_rejected_without_killing_the_daemon() {
    let harness = harness();
    let handle = start_daemon(&harness);
    let (identity, mut client) = wait_for_daemon(&harness);

    let mut connection = fact_ipc::client_connect(&identity.pipe).expect("raw connection");
    fact_ipc::send_client(
        &mut connection,
        &ClientMessage::Hello(Hello {
            protocol: fact_protocol::PROTOCOL_VERSION,
            client_version: "test".into(),
            token: "0".repeat(64),
        }),
    )
    .expect("send bad hello");
    match fact_ipc::recv_server(&mut connection).expect("read rejection") {
        ServerMessage::Fatal(error) => {
            assert_eq!(error.code, ErrorCode::Unauthorized);
        }
        other => panic!("expected Fatal, got {other:?}"),
    }
    drop(connection);

    match client.request(Command::Ping).expect("ping after rejection") {
        ResponsePayload::Pong { live_sessions, .. } => assert_eq!(live_sessions, 0),
        other => panic!("unexpected ping response: {other:?}"),
    }

    shutdown(client, handle, &harness);
}
