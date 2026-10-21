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
