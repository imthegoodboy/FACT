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
    // CI uses the test build. Set FACT_SMOKE_EXE to drive a downloaded release
    // binary through the same real-ConPTY checks.
    std::env::var_os("FACT_SMOKE_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_fact")))
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
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let powershell = format!(
            "{}/System32/WindowsPowerShell/v1.0/powershell.exe",
            system_root.replace('\\', "/")
        );
        fs::write(
            path.join("config.toml"),
            format!(
                "config_version = 1\nprefix = \"ctrl+b\"\ndefault_profile = \"test-shell\"\n\n[profiles.test-shell]\ncommand = \"{powershell}\"\nargs = [\"-NoLogo\", \"-NoProfile\"]\n"
            ),
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

fn marker_pid(output: &[u8], marker: &str) -> Option<u32> {
    let text = String::from_utf8_lossy(output);
    text.split(marker).skip(1).find_map(|rest| {
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    })
}

fn wait_for_marker_pid(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    marker: &str,
    timeout: Duration,
) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        drain(process, output);
        if let Some(pid) = marker_pid(output, marker) {
            return Some(pid);
        }
        if process.exit().is_some() || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn tui_cold_recovers_and_starts_new_shells() {
    const COLD_SESSION: &str = "cold-smoke";
    const OLD_MARKER: &str = "FACT_OLD_PID_";
    const NEW_MARKER: &str = "FACT_NEW_PID_";

    let home = TestHome::new("tui-cold");
    let mut options = SpawnOptions::new(fact_exe().to_string_lossy().into_owned(), 120, 35);
    options.args = vec![
        "--data-dir".into(),
        home.path.to_string_lossy().into_owned(),
        "--session".into(),
        COLD_SESSION.into(),
        "--grid".into(),
        "1".into(),
    ];
    let first = PtyProcess::spawn(options).expect("start the first FACT TUI in ConPTY");
    let mut output = Vec::new();
    assert!(
        wait_for(&first, &mut output, COLD_SESSION, Duration::from_secs(30)),
        "the first TUI did not start:\n{}",
        tail(&output)
    );
    assert!(
        wait_for(&first, &mut output, "pid ", Duration::from_secs(30)),
        "the first TUI did not render a live pane:\n{}",
        tail(&output)
    );

    first
        .write(format!("echo {OLD_MARKER}$PID\r").as_bytes())
        .expect("print the first shell PID");
    let old_pid = wait_for_marker_pid(&first, &mut output, OLD_MARKER, Duration::from_secs(20))
        .unwrap_or_else(|| panic!("the shell did not print its PID:\n{}", tail(&output)));

    first.write(b"\x02d").expect("detach the first TUI");
    let exit = first.wait_for_exit(Duration::from_secs(15));
    assert_eq!(
        exit,
        Some(0),
        "the first TUI did not detach cleanly:\n{}",
        tail(&output)
    );

    home.shutdown();
    assert!(
        !home.path.join("runtime").join("daemon.json").exists(),
        "the daemon did not stop before cold recovery"
    );

    let mut options = SpawnOptions::new(fact_exe().to_string_lossy().into_owned(), 120, 35);
    options.args = vec![
        "--data-dir".into(),
        home.path.to_string_lossy().into_owned(),
        "--session".into(),
        COLD_SESSION.into(),
    ];
    let second = PtyProcess::spawn(options).expect("resume the session in ConPTY");
    let mut output = Vec::new();
    assert!(
        wait_for(
            &second,
            &mut output,
            "Session recovered",
            Duration::from_secs(30)
        ),
        "cold recovery did not report the lost process:\n{}",
        tail(&output)
    );
    // Dismiss the recovery modal, then prove that a new shell is accepting
    // input. The old process must not have been resurrected. Restored visual
    // content is asserted by the daemon cold-recovery test, which searches the
    // terminal buffer directly rather than relying on the visible screen.
    second.write(b"\r").expect("dismiss the recovery modal");
    assert!(
        wait_for(
            &second,
            &mut output,
            "Continuing in the recovered session.",
            Duration::from_secs(10)
        ),
        "the recovery modal did not close:\n{}",
        tail(&output)
    );
    thread::sleep(Duration::from_millis(300));
    second
        .write(format!("echo {NEW_MARKER}$PID\r").as_bytes())
        .expect("print the new shell PID");
    let new_pid = wait_for_marker_pid(&second, &mut output, NEW_MARKER, Duration::from_secs(20))
        .unwrap_or_else(|| {
            panic!(
                "the recovered shell did not accept input:\n{}",
                tail(&output)
            )
        });
    assert_ne!(new_pid, old_pid, "cold recovery must start a new shell");

    second.write(b"\x02d").expect("detach the recovered TUI");
    let exit = second.wait_for_exit(Duration::from_secs(15));
    assert_eq!(
        exit,
        Some(0),
        "the recovered TUI did not detach cleanly:\n{}",
        tail(&output)
    );
    home.shutdown();
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
        "8".into(),
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
    let summary = parsed
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|session| session.get("name").and_then(Value::as_str) == Some(SESSION))
        .expect("the detached session is missing from fact sessions");
    assert_eq!(
        summary.get("live").and_then(Value::as_bool),
        Some(true),
        "detaching from the UI must leave the session running"
    );
    assert_eq!(
        summary.get("pane_count").and_then(Value::as_u64),
        Some(8),
        "the CLI --grid 8 launch must create eight panes"
    );

    let shutdown = home.run(&["shutdown"]);
    assert!(
        shutdown.status.success(),
        "shutdown failed: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
}
