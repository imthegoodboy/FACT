//! End-to-end CLI tests that do not need a running daemon.
//!
//! They use a throwaway data directory so the developer's real FACT state is
//! never touched.

#![cfg(windows)]

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use fact_layout::SessionLayout;
use fact_session::{PaneRecord, SessionData, SessionStore};
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
            .arg("--data-dir")
            .arg(&self.path)
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

    fn save_sample_session(&self) {
        let paths =
            fact_config::Paths::from_parts(self.path.clone(), self.path.join("config.toml"));
        paths.ensure().expect("create data directories");
        let mut store = SessionStore::open(&paths.db_file).expect("open session store");
        let layout = SessionLayout::new(1, &[10, 11]);
        let session = SessionData::new(
            "dev",
            layout,
            vec![
                PaneRecord {
                    id: 10,
                    title: "PowerShell".into(),
                    pid: 1234,
                    exit_code: None,
                    profile: Some("auto".into()),
                    cwd: Some(r"C:\Users\test".into()),
                },
                PaneRecord {
                    id: 11,
                    title: "cmd".into(),
                    pid: 1235,
                    exit_code: Some(0),
                    profile: None,
                    cwd: None,
                },
            ],
        );
        store.save(&session).expect("save sample session");
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn session_names(json: &str) -> Vec<String> {
    let sessions: Value = serde_json::from_str(json).expect("parse sessions JSON");
    sessions
        .as_array()
        .expect("sessions are a JSON array")
        .iter()
        .map(|session| session["name"].as_str().expect("session name").to_string())
        .collect()
}

#[test]
fn help_lists_every_required_command() {
    let home = TestHome::new("help");
    let help = home.ok(["--help"]);
    for command in [
        "resume",
        "sessions",
        "attach",
        "detach",
        "doctor",
        "rename",
        "save",
        "export",
        "import",
        "archive",
        "unarchive",
        "delete",
        "terminate",
        "stats",
        "shutdown",
        "config",
    ] {
        assert!(
            help.contains(command),
            "`fact --help` did not mention {command}:\n{help}"
        );
    }
    assert!(help.contains("--grid"), "grid option missing from help");
    assert!(
        help.contains("--session"),
        "session option missing from help"
    );

    let version = home.ok(["--version"]);
    assert!(
        version.starts_with("fact "),
        "unexpected version: {version}"
    );
}

#[test]
fn config_commands_validate_init_and_report_errors() {
    let home = TestHome::new("config");

    let path = home.ok(["config", "path"]);
    assert!(
        path.trim().ends_with("config.toml"),
        "unexpected path: {path}"
    );
    assert_eq!(PathBuf::from(path.trim()), home.config_file());

    let before: Value =
        serde_json::from_str(&home.ok(["config", "validate", "--json"])).expect("parse JSON");
    assert_eq!(before["valid"], true);
    assert_eq!(before["built_in_defaults"], true);
    assert_eq!(before["version"], 1);
    assert!(before["keybind_count"].as_u64().unwrap() >= 20);
    assert!(!before["default_shell"].as_str().unwrap_or("").is_empty());

    let written = home.ok(["config", "init"]);
    assert!(
        written.contains("Wrote"),
        "unexpected init output: {written}"
    );
    assert!(home.config_file().is_file());
    let content = fs::read_to_string(home.config_file()).expect("read config");
    assert!(content.contains("prefix = \"ctrl+space\""));

    let duplicate = home.run(["config", "init"]);
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("already exists"),
        "unexpected duplicate-init error: {}",
        String::from_utf8_lossy(&duplicate.stderr)
    );

    let after: Value =
        serde_json::from_str(&home.ok(["config", "validate", "--json"])).expect("parse JSON");
    assert_eq!(after["valid"], true);
    assert_eq!(after["built_in_defaults"], false);
    assert_eq!(after["prefix"], "ctrl+space");

    fs::write(
        home.config_file(),
        "config_version = 1\nprefix = \"banana\"\n",
    )
    .expect("write invalid config");
    let invalid = home.run(["config", "validate"]);
    assert!(!invalid.status.success(), "invalid prefix was accepted");
    assert!(!invalid.stderr.is_empty());
    let explicit = home.run(["config", "validate", "--json"]);
    assert!(!explicit.status.success());

    home.ok(["config", "init", "--force"]);
    let restored: Value =
        serde_json::from_str(&home.ok(["config", "validate", "--json"])).expect("parse JSON");
    assert_eq!(restored["valid"], true);
    assert_eq!(restored["built_in_defaults"], false);
}

#[test]
fn sessions_are_empty_before_the_first_run() {
    let home = TestHome::new("empty");
    assert_eq!(
        session_names(&home.ok(["sessions", "--json"])),
        Vec::<String>::new()
    );
    let text = home.ok(["sessions"]);
    assert!(text.contains("No sessions"), "unexpected output: {text}");
}

#[test]
fn offline_session_lifecycle_and_export_import() {
    let home = TestHome::new("lifecycle");
    home.save_sample_session();

    let json: Value =
        serde_json::from_str(&home.ok(["sessions", "--json"])).expect("parse sessions");
    let sessions = json.as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["name"], "dev");
    assert_eq!(sessions[0]["live"], false);
    assert_eq!(sessions[0]["pane_count"], 2);
    assert_eq!(sessions[0]["tab_count"], 1);

    let stdout_json = home.ok(["export", "dev", "--stdout"]);
    let exported: Value = serde_json::from_str(&stdout_json).expect("parse exported JSON");
    assert!(exported["format"]
        .as_str()
        .unwrap()
        .starts_with("fact-session"));
    assert_eq!(exported["session"]["name"], "dev");

    let exports = home.path.join("exports");
    fs::create_dir_all(&exports).expect("create export directory");
    let output = home.ok([
        "export",
        "dev",
        "--output",
        exports.to_str().expect("utf-8 export path"),
    ]);
    assert!(output.contains("Exported 'dev'"), "unexpected: {output}");
    let files: Vec<_> = fs::read_dir(&exports)
        .expect("read exports")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(files.len(), 1, "expected one export file, got {files:?}");
    let imported = home.ok([
        "import",
        files[0].to_str().expect("utf-8 export file"),
        "--name",
        "copy",
    ]);
    assert!(
        imported.contains("copy"),
        "unexpected import output: {imported}"
    );

    let names = session_names(&home.ok(["sessions", "--json"]));
    assert!(names.contains(&"dev".to_string()));
    assert!(names.contains(&"copy".to_string()));

    home.ok(["rename", "dev", "work"]);
    let names = session_names(&home.ok(["sessions", "--json"]));
    assert!(names.contains(&"work".to_string()));
    assert!(!names.contains(&"dev".to_string()));

    home.ok(["archive", "work"]);
    let names = session_names(&home.ok(["sessions", "--json"]));
    assert!(!names.contains(&"work".to_string()));
    let all = session_names(&home.ok(["sessions", "--all", "--json"]));
    assert!(all.contains(&"work".to_string()));

    home.ok(["unarchive", "work"]);
    let names = session_names(&home.ok(["sessions", "--json"]));
    assert!(names.contains(&"work".to_string()));

    home.ok(["delete", "work"]);
    home.ok(["delete", "copy"]);
    assert_eq!(
        session_names(&home.ok(["sessions", "--json"])),
        Vec::<String>::new()
    );
}

#[test]
fn doctor_reports_a_healthy_install_without_a_daemon() {
    let home = TestHome::new("doctor");
    let report: Value =
        serde_json::from_str(&home.ok(["doctor", "--json"])).expect("parse doctor JSON");
    assert_eq!(report["healthy"], true, "doctor report: {report:#}");
    let checks = report["checks"].as_array().expect("checks array");
    for required in [
        "data directory",
        "configuration",
        "default shell",
        "session database",
        "daemon",
        "ConPTY",
    ] {
        assert!(
            checks
                .iter()
                .any(|check| check["name"].as_str() == Some(required)),
            "doctor did not report {required}: {report:#}"
        );
    }
    let text = home.ok(["doctor"]);
    assert!(
        text.contains("No blocking problems found."),
        "unexpected doctor output:\n{text}"
    );
}
