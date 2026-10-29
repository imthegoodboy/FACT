//! Real-ConPTY integration test for the in-process engine.
//!
//! It starts the same engine the UI uses, resizes the workspace, types into a
//! shell and reads terminal cells back from the frame stream. No daemon is
//! involved.

#![cfg(windows)]

use std::thread;
use std::time::{Duration, Instant};

use fact_config::Config;
use fact_engine::{start, EventStream};
use fact_protocol::{Command, Frame, ServerMessage};

fn next_frame(events: &mut EventStream, timeout: Duration) -> Frame {
    let deadline = Instant::now() + timeout;
    loop {
        match events.available() {
            Ok(true) => match events.recv() {
                Ok(ServerMessage::Frame(frame)) => return *frame,
                Ok(ServerMessage::SessionEnded { reason }) => {
                    panic!("engine ended before a frame arrived: {reason}")
                }
                Err(error) => panic!("engine event stream failed: {error}"),
            },
            Ok(false) => {}
            Err(error) => panic!("engine event stream closed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an engine frame"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn frame_text(frame: &Frame) -> String {
    let mut text = String::new();
    for pane in &frame.panes {
        if let Some(cells) = &pane.cells {
            for cell in cells {
                text.push_str(&cell.symbol());
            }
            text.push('\n');
        }
    }
    text
}

#[test]
fn engine_starts_grid_types_and_stops_its_shells() {
    let started = start(Config::default(), 2, None, None).expect("start the engine");
    let client = started.client;
    let mut events = started.events;

    client
        .request(Command::ResizeClient {
            cols: 100,
            rows: 30,
        })
        .expect("resize the workspace");

    let mut frame = next_frame(&mut events, Duration::from_secs(20));
    assert_eq!(frame.panes.len(), 2, "grid 2 should own two shells");
    assert!(frame.panes.iter().all(|pane| pane.alive));

    let pane = frame.focus.unwrap_or(frame.panes[0].id);
    client
        .request(Command::Input {
            pane,
            data: b"echo FACT_ENGINE_OK\r".to_vec(),
        })
        .expect("write to the shell");

    let deadline = Instant::now() + Duration::from_secs(20);
    let found = loop {
        frame = next_frame(&mut events, Duration::from_secs(20));
        if frame_text(&frame).contains("FACT_ENGINE_OK") {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
    };
    assert!(found, "shell output never appeared in the frame stream");

    client.request(Command::Shutdown).expect("stop the engine");
    drop(started.engine);
}

/// Latest cells per pane, rebuilt from the delta frame stream like the UI does.
#[derive(Default)]
struct Screen {
    panes: std::collections::HashMap<u64, (u16, Vec<fact_protocol::CellDto>)>,
    modes: std::collections::HashMap<u64, u32>,
}

impl Screen {
    fn apply(&mut self, frame: Frame) {
        for pane in frame.panes {
            self.modes.insert(pane.id, pane.modes);
            if let Some(cells) = pane.cells {
                self.panes.insert(pane.id, (pane.content_cols, cells));
            }
        }
    }

    fn lines(&self, pane: u64) -> Vec<String> {
        let Some((cols, cells)) = self.panes.get(&pane) else {
            return Vec::new();
        };
        cells
            .chunks(usize::from(*cols).max(1))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Apply frames until `done` holds or the timeout passes.
    fn wait(
        &mut self,
        events: &mut EventStream,
        timeout: Duration,
        mut done: impl FnMut(&Screen) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match events.recv_timeout(left) {
                Ok(Some(ServerMessage::Frame(frame))) => self.apply(*frame),
                Ok(Some(ServerMessage::SessionEnded { reason })) => {
                    panic!("engine ended early: {reason}")
                }
                Ok(None) => {}
                Err(error) => panic!("engine event stream failed: {error}"),
            }
        }
        true
    }
}

/// ConPTY asks for win32-input-mode; keys sent that way keep modifiers that
/// VT bytes lose. Shift+Enter must reach PSReadLine as AddLine (a `>>`
/// continuation), not as a plain Enter that runs the first line.
#[test]
fn win32_input_mode_delivers_shift_enter_to_the_shell() {
    let mut config = Config::default();
    let powershell = format!(
        r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
        std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())
    );
    config.profiles.insert(
        "ps".into(),
        fact_config::Profile {
            command: Some(powershell),
            args: vec!["-NoLogo".into(), "-NoProfile".into()],
            enabled: true,
            ..fact_config::Profile::default()
        },
    );
    let started = start(config, 1, Some("ps".into()), None).expect("start the engine");
    let client = started.client;
    let mut events = started.events;
    client
        .request(Command::ResizeClient {
            cols: 100,
            rows: 30,
        })
        .expect("resize the workspace");

    let mut screen = Screen::default();
    assert!(
        screen.wait(&mut events, Duration::from_secs(30), |screen| {
            screen
                .modes
                .values()
                .any(|modes| modes & fact_protocol::pane_modes::WIN32_INPUT != 0)
                && screen
                    .panes
                    .keys()
                    .any(|pane| screen.lines(*pane).iter().any(|l| l.starts_with("PS ")))
        }),
        "ConPTY never requested win32-input-mode or the prompt never appeared"
    );
    let pane = *screen.panes.keys().next().unwrap();

    let started_at = Instant::now();
    client
        .request(Command::Input {
            pane,
            data: b"echo FACT_ECHO".to_vec(),
        })
        .expect("type");
    assert!(
        screen.wait(&mut events, Duration::from_secs(10), |screen| screen
            .lines(pane)
            .iter()
            .any(|line| line.contains("FACT_ECHO"))),
        "typed text was never echoed"
    );
    let echo = started_at.elapsed();
    eprintln!("keystroke echo through ConPTY and the engine: {echo:?}");
    assert!(echo < Duration::from_secs(2), "echo took {echo:?}");

    let shift_enter = b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_";
    let mut data = shift_enter.to_vec();
    data.extend_from_slice(b"echo FACT_SECOND\r");
    client
        .request(Command::Input { pane, data })
        .expect("send Shift+Enter");
    let ok = screen.wait(&mut events, Duration::from_secs(15), |screen| {
        let lines = screen.lines(pane);
        lines
            .iter()
            .any(|line| line.starts_with(">>") && line.contains("echo FACT_SECOND"))
            && lines.iter().any(|line| line == "FACT_SECOND")
            && lines.iter().any(|line| line == "FACT_ECHO")
    });
    assert!(
        ok,
        "Shift+Enter did not continue the command line:\n{}",
        screen.lines(pane).join("\n")
    );

    client.request(Command::Shutdown).expect("stop the engine");
    drop(started.engine);
}
