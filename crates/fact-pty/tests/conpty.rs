//! Real ConPTY integration tests. These start Windows shells; they are not
//! mocked and therefore only run on Windows.

#![cfg(windows)]

use fact_pty::{PtyProcess, SpawnOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn cmd() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string())
}

fn collect(process: &PtyProcess, timeout: Duration) -> (Vec<u8>, Option<i32>) {
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut exit = None;
    while Instant::now() < deadline {
        while let Some(chunk) = process.try_read() {
            output.extend_from_slice(&chunk);
        }
        exit = process.exit().map(|e| e.code);
        if exit.is_some() {
            // Drain any output already queued after process exit.
            while let Some(chunk) = process.try_read() {
                output.extend_from_slice(&chunk);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (output, exit)
}

#[test]
fn echo_marker_and_exit() {
    let mut options = SpawnOptions::new(cmd(), 80, 24);
    options.args = vec![
        "/D".into(),
        "/Q".into(),
        "/C".into(),
        "echo FACT_CONPTY_MARKER".into(),
    ];
    let process = PtyProcess::spawn(options).expect("spawn cmd through ConPTY");
    let (output, exit) = collect(&process, Duration::from_secs(15));
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("FACT_CONPTY_MARKER"), "output: {text:?}");
    assert_eq!(exit, Some(0));
    assert!(!process.is_alive());
}

#[test]
fn propagated_exit_code() {
    let mut options = SpawnOptions::new(cmd(), 60, 20);
    options.args = vec!["/D".into(), "/Q".into(), "/C".into(), "exit 7".into()];
    let process = PtyProcess::spawn(options).unwrap();
    assert_eq!(process.wait_for_exit(Duration::from_secs(15)), Some(7));
}

#[test]
fn interactive_input_is_delivered() {
    let mut options = SpawnOptions::new(cmd(), 90, 30);
    options.args = vec!["/D".into(), "/Q".into()];
    options.cwd = Some(PathBuf::from(
        std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\".to_string()),
    ));
    let process = PtyProcess::spawn(options).unwrap();
    std::thread::sleep(Duration::from_millis(250));
    process
        .write(b"echo FACT_INPUT_MARKER\r\nexit /b 0\r\n")
        .unwrap();
    let (output, exit) = collect(&process, Duration::from_secs(15));
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("FACT_INPUT_MARKER"), "output: {text:?}");
    assert_eq!(exit, Some(0));
}

#[test]
fn resize_then_continue() {
    let mut options = SpawnOptions::new(cmd(), 80, 24);
    options.args = vec!["/D".into(), "/Q".into()];
    let process = PtyProcess::spawn(options).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    process.resize(120, 40).unwrap();
    // ConPTY rebuilds the console around a resize; input sent in the same
    // instant can be discarded. Give the resize a moment to settle, as a real
    // client does between redrawing and accepting the next keystroke.
    std::thread::sleep(Duration::from_millis(300));
    process.write(b"echo FACT_RESIZED\r\nexit\r\n").unwrap();
    let (output, _) = collect(&process, Duration::from_secs(15));
    assert!(String::from_utf8_lossy(&output).contains("FACT_RESIZED"));
}

#[test]
fn eight_independent_shells() {
    let mut processes = Vec::new();
    for index in 0..8 {
        let mut options = SpawnOptions::new(cmd(), 80, 24);
        options.args = vec![
            "/D".into(),
            "/Q".into(),
            "/C".into(),
            format!("echo FACT_GRID_{index}"),
        ];
        processes.push((index, PtyProcess::spawn(options).unwrap()));
    }
    for (index, process) in &processes {
        assert!(process.pid() > 0);
        let (output, exit) = collect(process, Duration::from_secs(20));
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains(&format!("FACT_GRID_{index}")),
            "pane {index}: {text:?}"
        );
        assert_eq!(exit, Some(0));
    }
}

#[test]
fn larger_output_is_not_truncated_by_channels() {
    let mut options = SpawnOptions::new(cmd(), 100, 30);
    options.args = vec![
        "/D".into(),
        "/Q".into(),
        "/C".into(),
        "for /L %i in (1,1,4000) do @echo 0123456789012345678901234567890123456789".into(),
    ];
    let process = PtyProcess::spawn(options).unwrap();
    let (output, exit) = collect(&process, Duration::from_secs(30));
    assert!(output.len() > 100_000, "only {} bytes", output.len());
    assert_eq!(exit, Some(0));
}

#[test]
fn no_shell_is_left_behind_after_kill() {
    let mut options = SpawnOptions::new(cmd(), 80, 24);
    options.args = vec!["/D".into(), "/Q".into()];
    let process = PtyProcess::spawn(options).unwrap();
    let pid = process.pid();
    std::thread::sleep(Duration::from_millis(200));
    process.kill();
    assert!(process.wait_for_exit(Duration::from_secs(10)).is_some());
    // PIDs can be reused, so only check that the process object reported exit.
    assert!(!process.is_alive(), "pid {pid} should no longer be running");
}
