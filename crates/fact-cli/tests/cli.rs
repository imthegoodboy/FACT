//! End-to-end CLI tests for argument parsing and configuration commands.
//!
//! The interactive terminal is not started here. Each test points FACT at a
//! throwaway directory so the developer's real configuration is never touched.

#![cfg(windows)]

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

fn fact_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fact"))
}

struct TestHome {
    path: PathBuf,
}

impl TestHome {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fact-cli-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test home");
        Self { path }
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(fact_exe())
            .env("FACT_HOME", &self.path)
            .args(args)
            .output()
            .expect("run fact")
    }

    fn ok<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "fact failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn config_file(&self) -> PathBuf {
        self.path.join("config.toml")
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn json(output: &str) -> Value {
    serde_json::from_str(output).expect("parse JSON")
}

#[test]
fn help_describes_the_single_process_workspace() {
    let home = TestHome::new("help");
    let help = home.ok(["--help"]);
    for expected in [
        "--grid",
        "--profile",
        "--cwd",
        "--config",
        "config",
        "workspace",
    ] {
        assert!(
            help.contains(expected),
            "`fact --help` did not mention {expected}:\n{help}"
        );
    }
    for obsolete in [
        "resume", "sessions", "attach", "detach", "daemon", "shutdown",
    ] {
        assert!(
            !help.contains(obsolete),
            "`fact --help` still mentions {obsolete}:\n{help}"
        );
    }

    let version = home.ok(["--version"]);
    assert!(
        version.starts_with("fact "),
        "unexpected version: {version}"
    );
}

#[test]
fn config_path_uses_fact_home() {
    let home = TestHome::new("path");
    let path = home.ok(["config", "path"]);
    assert_eq!(PathBuf::from(path.trim()), home.config_file());
}

#[test]
fn config_validate_reports_built_in_defaults() {
    let home = TestHome::new("defaults");
    let value = json(&home.ok(["config", "validate", "--json"]));
    assert_eq!(value["valid"], true);
    assert_eq!(value["built_in_defaults"], true);
    assert_eq!(value["version"], 2);
    assert_eq!(value["prefix"], "ctrl+space");
    assert!(value["keybind_count"].as_u64().unwrap() >= 20);
    assert!(!value["default_shell"].as_str().unwrap_or("").is_empty());

    let text = home.ok(["config", "validate"]);
    assert!(
        text.contains("built-in defaults"),
        "unexpected output: {text}"
    );
}

#[test]
fn config_init_writes_a_valid_file_and_refuses_overwrites() {
    let home = TestHome::new("init");
    let written = home.ok(["config", "init"]);
    assert!(
        written.contains("Wrote"),
        "unexpected init output: {written}"
    );
    assert!(home.config_file().is_file());
    let content = fs::read_to_string(home.config_file()).expect("read config");
    assert!(content.contains("config_version = 2"));
    assert!(content.contains("prefix = \"ctrl+space\""));

    let after = json(&home.ok(["config", "validate", "--json"]));
    assert_eq!(after["valid"], true);
    assert_eq!(after["built_in_defaults"], false);

    let duplicate = home.run(["config", "init"]);
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("already exists"),
        "unexpected duplicate-init error: {}",
        String::from_utf8_lossy(&duplicate.stderr)
    );

    home.ok(["config", "init", "--force"]);
    let restored = json(&home.ok(["config", "validate", "--json"]));
    assert_eq!(restored["valid"], true);
}

#[test]
fn config_validate_rejects_an_invalid_prefix() {
    let home = TestHome::new("invalid");
    fs::write(
        home.config_file(),
        "config_version = 2\nprefix = \"banana\"\n",
    )
    .expect("write invalid config");

    let invalid = home.run(["config", "validate"]);
    assert!(!invalid.status.success(), "invalid prefix was accepted");
    assert!(
        !invalid.stderr.is_empty(),
        "invalid prefix produced no diagnostic"
    );

    let explicit = home.run(["config", "validate", "--json"]);
    assert!(!explicit.status.success());
}

#[test]
fn obsolete_daemon_settings_are_rejected() {
    let home = TestHome::new("obsolete");
    fs::write(home.config_file(), "config_version = 2\nmax_sessions = 8\n")
        .expect("write obsolete config");
    let output = home.run(["config", "validate"]);
    assert!(!output.status.success(), "obsolete setting was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("max_sessions"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
