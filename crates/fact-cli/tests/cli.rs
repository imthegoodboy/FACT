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
        // Never touch the real Windows Terminal fragments folder.
        Command::new(fact_exe())
            .env("FACT_HOME", &self.path)
            .env("FACT_WT_FRAGMENTS_DIR", self.fragments_dir())
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

    fn fragments_dir(&self) -> PathBuf {
        self.path.join("wt-fragments")
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
        "--transparent",
        "config",
        "wallpaper",
        "doctor",
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
    assert_eq!(value["background"], "theme");
    assert_eq!(value["alt_shortcuts"], false);

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

#[test]
fn wallpaper_set_show_and_clear_use_a_windows_terminal_fragment() {
    let home = TestHome::new("wallpaper");
    let none = home.ok(["wallpaper"]);
    assert!(none.contains("No wallpaper set"), "unexpected: {none}");

    let image = home.path.join("beach.png");
    fs::write(&image, b"png bytes").expect("write image");
    let set = home.ok([
        "wallpaper".as_ref(),
        "set".as_ref(),
        image.as_os_str(),
        "--opacity".as_ref(),
        "25%".as_ref(),
        "--fit".as_ref(),
        "fit".as_ref(),
    ]);
    assert!(set.contains("wt -p FACT"), "unexpected: {set}");

    let fragment = home.fragments_dir().join("FACT").join("fact.json");
    let profile = &json(&fs::read_to_string(&fragment).expect("fragment"))["profiles"][0];
    assert_eq!(profile["name"], "FACT");
    assert_eq!(profile["backgroundImageOpacity"], 0.25);
    assert_eq!(profile["backgroundImageStretchMode"], "uniform");
    let copied = PathBuf::from(profile["backgroundImage"].as_str().unwrap());
    assert!(
        copied.starts_with(&home.path),
        "image copy is outside FACT_HOME"
    );
    assert!(copied.is_file());
    let commandline = profile["commandline"].as_str().unwrap();
    assert!(commandline.contains("fact") && commandline.ends_with("--transparent"));

    let shown = home.ok(["wallpaper"]);
    assert!(
        shown.contains("25%") && shown.contains("fit"),
        "unexpected: {shown}"
    );

    let bad = home.run(["wallpaper", "set", "missing.png"]);
    assert!(!bad.status.success());

    let cleared = home.ok(["wallpaper", "clear"]);
    assert!(cleared.contains("removed"), "unexpected: {cleared}");
    assert!(!fragment.exists());
    assert!(!copied.exists());
}

#[test]
fn doctor_reports_without_changing_anything() {
    let home = TestHome::new("doctor");
    fs::create_dir_all(home.path.join("runtime")).expect("legacy dir");
    let report = home.ok(["doctor"]);
    for expected in ["FACT", "Terminal", "Config", "Wallpaper", "runtime"] {
        assert!(
            report.contains(expected),
            "doctor did not mention {expected}:\n{report}"
        );
    }
    assert!(
        home.path.join("runtime").is_dir(),
        "doctor must not delete files"
    );
    assert!(!home.config_file().exists(), "doctor must not write config");
}
