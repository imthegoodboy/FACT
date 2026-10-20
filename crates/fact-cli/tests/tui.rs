//! End-to-end smoke test for the real TUI running in a real ConPTY.
//!
//! The test drives the same executable a user runs, creates real shells through
//! the daemon, verifies that rendering and keyboard input work, then detaches
//! and checks that the daemon and its shells survived.

#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use fact_pty::{PtyProcess, SpawnOptions};
use serde_json::Value;

const SESSION: &str = "ui-smoke";

fn fact_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fact"))
}

struct TestHome {
    path: PathBuf,
}

impl TestHome {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fact-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test home");
        fs::write(
            path.join("config.toml"),
            "config_version = 1\nprefix = \"ctrl+b\"\n",
        )
        .expect("write config");
        Self { path }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(fact_exe())
            .arg("--data-dir")
            .arg(&self.path)
            .args(args)
            .output()
            .expect("run fact")
    }

    fn shutdown(&self) {
        let _ = self.run(&["shutdown"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !self.path.join("runtime").join("daemon.json").exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        self.shutdown();
        for _ in 0..50 {
            if fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn drain(process: &PtyProcess, output: &mut Vec<u8>) {
    while let Some(chunk) = process.try_read() {
        output.extend_from_slice(&chunk);
    }
}

fn wait_for(process: &PtyProcess, output: &mut Vec<u8>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        drain(process, output);
        if String::from_utf8_lossy(output).contains(needle) {
            return true;
        }
        if process.exit().is_some() || Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn tail(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let reversed: String = text.chars().rev().take(6000).collect();
    reversed.chars().rev().collect()
}

#[test]
fn tui_renders_accepts_input_and_detaches_cleanly() {
    let home = TestHome::new("tui");
    let mut options = SpawnOptions::new(fact_exe().to_string_lossy().into_owned(), 120, 35);
    options.args = vec![
        "--data-dir".into(),
        home.path.to_string_lossy().into_owned(),
        "--session".into(),
        SESSION.into(),
        "--grid".into(),
        "2".into(),
    ];
    let process = PtyProcess::spawn(options).expect("start the FACT TUI in ConPTY");
    let mut output = Vec::new();

    assert!(
        wait_for(&process, &mut output, SESSION, Duration::from_secs(30)),
        "the TUI did not start:\n{}",
        tail(&output)
    );
    assert!(
        wait_for(&process, &mut output, "pid ", Duration::from_secs(30)),
        "the TUI did not render a live pane:\n{}",
        tail(&output)
    );

    process
        .write(b"echo FACT_TUI_INPUT\r")
        .expect("type into the focused pane");
    assert!(
        wait_for(
            &process,
            &mut output,
            "FACT_TUI_INPUT",
            Duration::from_secs(20)
        ),
        "keyboard input did not reach the shell:\n{}",
        tail(&output)
    );

    process.write(b"\x02k").expect("open the command palette");
    assert!(
        wait_for(&process, &mut output, "Commands", Duration::from_secs(10)),
        "the command palette did not open:\\n{}",
        tail(&output)
    );
    process.write(b"\x1b").expect("close the command palette");
    thread::sleep(Duration::from_millis(250));

    process.write(b"\x02/").expect("open pane search");
    assert!(
        wait_for(
            &process,
            &mut output,
            "Search pane",
            Duration::from_secs(10)
        ),
        "the search overlay did not open:\\n{}",
        tail(&output)
    );
    process
        .write(b"FACT_TUI_INPUT\r")
        .expect("search the pane scrollback");
    assert!(
        wait_for(&process, &mut output, "hit", Duration::from_secs(10)),
        "search did not return a result:\\n{}",
        tail(&output)
    );
    process.write(b"\x1b").expect("close the search overlay");
    thread::sleep(Duration::from_millis(250));

    process.write(b"\x1bOP").expect("press F1");
    assert!(
        wait_for(
            &process,
            &mut output,
            "Keyboard help",
            Duration::from_secs(10)
        ),
        "F1 did not open the help overlay:\n{}",
        tail(&output)
    );

    // Esc closes help. Give crossterm a moment to disambiguate the solitary
    // escape from an escape sequence before arming the prefix.
    process.write(b"\x1b").expect("close help");
    thread::sleep(Duration::from_millis(400));
    process.write(b"\x02d").expect("prefix then detach");

    let exit = process.wait_for_exit(Duration::from_secs(15));
    for _ in 0..20 {
        drain(&process, &mut output);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        exit,
        Some(0),
        "the TUI did not exit cleanly:\n{}",
        tail(&output)
    );
    assert!(
        String::from_utf8_lossy(&output).contains("Detached from 'ui-smoke'"),
        "the detach message is missing:\n{}",
        tail(&output)
    );

    let sessions = home.run(&["sessions", "--json"]);
    assert!(
        sessions.status.success(),
        "fact sessions failed: {}",
        String::from_utf8_lossy(&sessions.stderr)
    );
    let parsed: Value = serde_json::from_slice(&sessions.stdout).expect("sessions JSON");
    let live = parsed
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|session| session.get("name").and_then(Value::as_str) == Some(SESSION))
        .and_then(|session| session.get("live"))
        .and_then(Value::as_bool);
    assert_eq!(
        live,
        Some(true),
        "detaching from the UI must leave the session running"
    );

    let shutdown = home.run(&["shutdown"]);
    assert!(
        shutdown.status.success(),
        "shutdown failed: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
}
