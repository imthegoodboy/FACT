//! Starting and reaching the per-user daemon.
//!
//! The CLI never embeds the daemon loop: it spawns the same executable with the
//! hidden `daemon` subcommand in a detached process and then waits for the
//! authenticated identity file to become usable.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use fact_config::Paths;
use fact_runtime::{connect, DaemonClient, DaemonIdentity, RuntimeError};

const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Connect to the daemon, starting a detached one if the current data
/// directory does not have a live daemon.
pub fn ensure_daemon(
    paths: &Paths,
    config_override: Option<&PathBuf>,
) -> Result<(DaemonIdentity, DaemonClient)> {
    match connect(paths) {
        Ok(connected) => return Ok(connected),
        Err(error) if should_start(&error) => {}
        Err(error) => return Err(error).context("could not connect to the FACT daemon"),
    }

    paths
        .ensure()
        .with_context(|| format!("could not create data directory {}", paths.base.display()))?;
    let mut child = spawn_daemon(paths, config_override)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_error = None;

    while Instant::now() < deadline {
        match connect(paths) {
            Ok(connected) => return Ok(connected),
            Err(error) if should_start(&error) => {
                last_error = Some(error.to_string());
            }
            Err(error) => {
                let _ = child.kill();
                return Err(error).context(
                    "the daemon started but authentication failed; run `fact doctor` for details",
                );
            }
        }

        if let Some(status) = child.try_wait().context("could not check daemon process")? {
            // A second client may have won the race to bind the pipe. Give the
            // winner one final chance before reporting the failed child.
            thread::sleep(Duration::from_millis(100));
            if let Ok(connected) = connect(paths) {
                return Ok(connected);
            }
            bail!(
                "daemon exited during startup ({status}).\n{}",
                startup_log_tail(paths)
            );
        }
        thread::sleep(Duration::from_millis(75));
    }

    let _ = child.kill();
    bail!(
        "daemon did not become ready within {}s{}.\n{}",
        START_TIMEOUT.as_secs(),
        last_error
            .map(|error| format!(" (last error: {error})"))
            .unwrap_or_default(),
        startup_log_tail(paths)
    )
}

/// Connect only if a daemon is already running. Used by `attach`, `doctor` and
/// session commands that must not silently create a server.
pub fn existing_daemon(paths: &Paths) -> Result<(DaemonIdentity, DaemonClient)> {
    connect(paths).map_err(|error| match error {
        RuntimeError::NotRunning(_) | RuntimeError::Ipc(_) | RuntimeError::Io(_) => {
            anyhow::anyhow!("no FACT daemon is running in {}", paths.base.display())
        }
        other => anyhow::Error::new(other).context("could not connect to the FACT daemon"),
    })
}

/// Connect if possible, returning `None` when no daemon is running. Errors that
/// indicate a live but misconfigured daemon are still reported.
pub fn optional_daemon(paths: &Paths) -> Result<Option<(DaemonIdentity, DaemonClient)>> {
    match connect(paths) {
        Ok(connected) => Ok(Some(connected)),
        Err(error) if should_start(&error) => Ok(None),
        Err(error) => Err(error).context("could not connect to the FACT daemon"),
    }
}

/// Entry point for the hidden `daemon` subcommand.
pub fn run_daemon(paths: Paths) -> Result<()> {
    fact_daemon::run(paths).context("daemon stopped with an error")
}

fn should_start(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::NotRunning(_) | RuntimeError::Ipc(_) | RuntimeError::Io(_)
    )
}

fn spawn_daemon(paths: &Paths, config_override: Option<&PathBuf>) -> Result<Child> {
    let executable = std::env::current_exe().context("could not locate the FACT executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.daemon_log)
        .with_context(|| format!("could not open daemon log {}", paths.daemon_log.display()))?;

    let mut command = Command::new(&executable);
    command
        .arg("daemon")
        .arg("--data-dir")
        .arg(&paths.base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    if let Some(config) = config_override {
        command.arg("--config").arg(config);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Detached: the daemon must survive the terminal that launched it.
        // New process group: a Ctrl+C in the parent console must not hit it.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    command
        .spawn()
        .with_context(|| format!("could not start the FACT daemon ({})", executable.display()))
}

fn startup_log_tail(paths: &Paths) -> String {
    let tail = read_tail(&paths.daemon_log, 4096);
    if tail.trim().is_empty() {
        format!(
            "No daemon log was written. Try running `{} daemon` in a console for details.",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "fact".into())
        )
    } else {
        format!("--- daemon log (tail) ---\n{}", tail.trim_end())
    }
}

fn read_tail(path: &std::path::Path, limit: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(limit)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).replace('\0', "")
}
