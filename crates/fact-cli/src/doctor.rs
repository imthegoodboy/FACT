//! `fact doctor`: local health checks with actionable output.

use std::time::Duration;

use anyhow::Result;
use fact_config::{Config, Paths};
use fact_protocol::{Command, ResponsePayload};
use fact_runtime::read_identity;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: String,
    pub data_dir: String,
    pub config_file: String,
    pub checks: Vec<Check>,
    pub healthy: bool,
}

impl Report {
    pub fn push(&mut self, name: impl Into<String>, level: Level, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            level,
            detail: detail.into(),
        });
    }
}

const TAIL_BYTES: usize = 1800;

pub fn run(paths: &Paths, json: bool) -> Result<bool> {
    let mut report = Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: paths.base.display().to_string(),
        config_file: paths.config_file.display().to_string(),
        checks: Vec::new(),
        healthy: true,
    };

    check_data_dir(paths, &mut report);
    let config = check_config(paths, &mut report);
    if let Some(config) = &config {
        check_shell(config, &mut report);
    }
    check_database(paths, &mut report);
    check_identity(paths, &mut report);
    check_log(paths, &mut report);
    check_pty(&mut report);

    report.healthy = !report.checks.iter().any(|check| check.level == Level::Fail);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("FACT {}  data: {}", report.version, report.data_dir);
        println!();
        for check in &report.checks {
            println!(
                "  {:<5} {:<22} {}",
                check.level.label(),
                check.name,
                check.detail
            );
        }
        println!();
        println!(
            "{}",
            if report.healthy {
                "No blocking problems found."
            } else {
                "Doctor found blocking problems; see `fact doctor --json` for details."
            }
        );
    }
    Ok(report.healthy)
}

fn check_data_dir(paths: &Paths, report: &mut Report) {
    match paths.ensure() {
        Ok(()) => {
            let probe = paths.base.join(format!(".doctor-{}", std::process::id()));
            match std::fs::write(&probe, b"fact doctor probe") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                    report.push("data directory", Level::Ok, "exists and is writable");
                }
                Err(error) => report.push(
                    "data directory",
                    Level::Fail,
                    format!("not writable: {error}"),
                ),
            }
        }
        Err(error) => report.push(
            "data directory",
            Level::Fail,
            format!("could not create {}: {error}", paths.base.display()),
        ),
    }
}

fn check_config(paths: &Paths, report: &mut Report) -> Option<Config> {
    let exists = paths.config_file.is_file();
    match Config::load(paths) {
        Ok(config) => {
            report.push(
                "configuration",
                Level::Ok,
                format!(
                    "valid{} ({} profile{})",
                    if exists { "" } else { ", built-in defaults" },
                    config.profiles.len(),
                    if config.profiles.len() == 1 { "" } else { "s" }
                ),
            );
            Some(config)
        }
        Err(error) => {
            report.push("configuration", Level::Fail, error.to_string());
            None
        }
    }
}

fn check_shell(config: &Config, report: &mut Report) {
    match config.resolve_profile(None) {
        Ok(profile) => {
            let cwd = profile
                .cwd
                .as_ref()
                .filter(|path| path.is_dir())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "daemon working directory".into());
            report.push(
                "default shell",
                Level::Ok,
                format!("{} ({cwd})", profile.command),
            );
        }
        Err(error) => report.push("default shell", Level::Fail, error.to_string()),
    }
    match config.compiled_keybinds() {
        Ok(bindings) => report.push(
            "keybindings",
            Level::Ok,
            format!(
                "{} actions, prefix {}",
                bindings.by_action.len(),
                bindings.prefix
            ),
        ),
        Err(error) => report.push("keybindings", Level::Fail, error.to_string()),
    }
}

fn check_database(paths: &Paths, report: &mut Report) {
    if !paths.db_file.is_file() {
        report.push(
            "session database",
            Level::Warn,
            "not created yet (will be created on first session)",
        );
        return;
    }
    match fact_session::SessionStore::open(&paths.db_file) {
        Ok(store) => match store.list(true) {
            Ok(sessions) => {
                let archived = sessions.iter().filter(|session| session.archived).count();
                report.push(
                    "session database",
                    Level::Ok,
                    format!(
                        "readable, {} saved session{} ({archived} archived)",
                        sessions.len(),
                        if sessions.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            Err(error) => report.push("session database", Level::Fail, error.to_string()),
        },
        Err(error) => report.push("session database", Level::Fail, error.to_string()),
    }
}

fn check_identity(paths: &Paths, report: &mut Report) {
    if !paths.daemon_json.exists() {
        report.push(
            "daemon",
            Level::Warn,
            "not running (run `fact resume` to start it)",
        );
        return;
    }
    match read_identity(paths) {
        Ok(identity) => {
            let acl = check_identity_acl(paths);
            match fact_runtime::connect(paths) {
                Ok((_, mut client)) => match client.request(Command::Ping) {
                    Ok(ResponsePayload::Pong {
                        daemon_pid,
                        uptime_ms,
                        live_sessions,
                    }) => {
                        report.push(
                            "daemon",
                            Level::Ok,
                            format!(
                                "pid {daemon_pid}, up {}, {live_sessions} live session{}",
                                crate::output::format_duration(uptime_ms),
                                if live_sessions == 1 { "" } else { "s" }
                            ),
                        );
                        if identity.pid != daemon_pid {
                            report.push(
                                "identity",
                                Level::Warn,
                                format!(
                                    "file says pid {} but pipe belongs to {daemon_pid}",
                                    identity.pid
                                ),
                            );
                        }
                        report.push("runtime ACL", acl.0, format!("{} {}", acl.1, identity.pipe));
                    }
                    Ok(other) => report.push(
                        "daemon",
                        Level::Fail,
                        format!("unexpected ping reply: {other:?}"),
                    ),
                    Err(error) => report.push("daemon", Level::Fail, error.to_string()),
                },
                Err(error) => {
                    report.push(
                        "daemon",
                        Level::Warn,
                        format!("identity file exists but daemon is unreachable: {error}"),
                    );
                    report.push("runtime ACL", acl.0, acl.1);
                }
            }
        }
        Err(error) => report.push("daemon identity", Level::Fail, error.to_string()),
    }
}

fn check_identity_acl(paths: &Paths) -> (Level, String) {
    #[cfg(windows)]
    {
        match (
            fact_ipc::file_dacl_sddl(&paths.daemon_json),
            fact_ipc::current_user_sid_string(),
        ) {
            (Ok(dacl), Ok(sid)) => {
                let broad = [";;;WD)", ";;;BU)", ";;;AU)", ";;;AN)"];
                let dangerous = broad.iter().any(|entry| dacl.contains(entry));
                if dacl.contains(&sid) && !dangerous {
                    (Level::Ok, format!("user-only DACL ({dacl})"))
                } else if dangerous {
                    (
                        Level::Fail,
                        format!("identity file may be readable by other users ({dacl})"),
                    )
                } else {
                    (
                        Level::Warn,
                        format!("DACL does not explicitly contain the current user ({dacl})"),
                    )
                }
            }
            (Err(error), _) => (Level::Warn, format!("could not read DACL: {error}")),
            (_, Err(error)) => (Level::Warn, format!("could not read user SID: {error}")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        (Level::Warn, "ACL diagnostics are Windows-only".into())
    }
}

fn check_log(paths: &Paths, report: &mut Report) {
    match std::fs::metadata(&paths.daemon_log) {
        Ok(meta) => {
            let mut detail = format!(
                "{} ({})",
                paths.daemon_log.display(),
                crate::output::format_bytes(meta.len())
            );
            if meta.len() > 8 * 1024 * 1024 {
                detail.push_str(" — larger than 8 MiB; rotate or remove it");
                report.push("daemon log", Level::Warn, detail);
            } else {
                report.push("daemon log", Level::Ok, detail);
            }
        }
        Err(_) => report.push("daemon log", Level::Warn, "not written yet"),
    }
    let tail = tail(&paths.daemon_log, TAIL_BYTES);
    if tail.to_ascii_lowercase().contains("panic") {
        report.push(
            "daemon log errors",
            Level::Warn,
            "log contains a panic; inspect the log tail",
        );
    }
}

fn check_pty(report: &mut Report) {
    let command = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    let mut options = fact_pty::SpawnOptions::new(&command, 40, 8);
    options.args = vec!["/d".into(), "/c".into(), "exit 0".into()];
    match fact_pty::PtyProcess::spawn(options) {
        Ok(process) => match process.wait_for_exit(Duration::from_secs(5)) {
            Some(0) => report.push(
                "ConPTY",
                Level::Ok,
                format!("real console process exited cleanly ({command})"),
            ),
            Some(code) => report.push(
                "ConPTY",
                Level::Warn,
                format!("console process exited with code {code}"),
            ),
            None => report.push(
                "ConPTY",
                Level::Warn,
                "console process did not exit within 5s",
            ),
        },
        Err(error) => report.push("ConPTY", Level::Fail, error.to_string()),
    }
}

fn tail(path: &std::path::Path, limit: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).replace('\0', "")
}
