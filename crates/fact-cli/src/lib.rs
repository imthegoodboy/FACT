//! FACT command line interface.

mod doctor;
mod output;
mod startup;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fact_config::{Config, Paths};
use fact_protocol::{AttachInfo, Command, DaemonStats, ResponsePayload, SessionSummary};
use fact_runtime::DaemonClient;

pub const DEFAULT_CONFIG: &str = r#"# FACT configuration (version 1)
# Every key is optional; omitted values use the built-in defaults.

config_version = 1
prefix = "ctrl+space"
theme = "fact-dark"
scrollback_lines = 8000
min_pane_width = 24
min_pane_height = 5
mouse = true
confirm_close = true
copy_on_select = false
persist_interval_secs = 10
snapshot_interval_secs = 20
snapshot_max_bytes = 262144
max_sessions = 32
max_frame_cells = 2000000
daemon_log_level = "info"
default_profile = "auto"

# Remap any action from the help overlay:
# [keybinds]
# split_right = "prefix+|"
# new_tab = "prefix+c"

# [profiles.pwsh]
# command = "pwsh.exe"
# args = ["-NoLogo"]
# cwd = "~"
"#;

#[derive(Debug, Parser)]
#[command(
    name = "fact",
    version,
    about = "FACT — a Windows-native terminal multiplexer",
    long_about = "FACT keeps real shells alive in a per-user daemon. Run `fact --grid 8` for a grid, \
                  `fact resume` to reattach, or `fact --help` for the command list.",
    propagate_version = true
)]
struct Cli {
    /// Data directory (defaults to %LOCALAPPDATA%\FACT, or FACT_HOME).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Configuration file (defaults to <data-dir>/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(flatten)]
    launch: LaunchArgs,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Args)]
struct LaunchArgs {
    /// Session to create, resume or attach to.
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
    /// Start a grid with this many independent shells (1-32).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=32))]
    grid: u8,
    /// Shell profile from the configuration.
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,
    /// Working directory for a new shell.
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ResumeArgs {
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=32))]
    grid: u8,
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create, warm-resume or reattach to a session (default command).
    Resume(ResumeArgs),
    /// Attach the TUI to an already-running session.
    Attach {
        /// Session name.
        name: String,
    },
    /// List saved and running sessions.
    Sessions {
        /// Include archived sessions.
        #[arg(long)]
        all: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask attached FACT UIs to detach without stopping their shells.
    Detach {
        /// Detach only clients of this session.
        #[arg(long, value_name = "NAME")]
        session: Option<String>,
    },
    /// Check the installation, config, database, ACLs, shell and ConPTY.
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rename a session (live or saved).
    Rename {
        /// Current name.
        name: String,
        /// New name.
        new_name: String,
    },
    /// Persist a live session's layout and visual snapshots now.
    Save {
        /// Session to save; defaults to the most recently used live session.
        name: Option<String>,
    },
    /// Export a session as versioned JSON.
    Export {
        /// Session to export; defaults to the most recently used session.
        name: Option<String>,
        /// Destination file or directory ("-" writes JSON to stdout).
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Write JSON to stdout instead of a file.
        #[arg(long)]
        stdout: bool,
    },
    /// Import a session export as a new saved session.
    Import {
        /// JSON file produced by `fact export`.
        file: PathBuf,
        /// Name for the imported copy.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Replace an existing saved session with the same name.
        #[arg(long)]
        overwrite: bool,
    },
    /// Hide a session from the default list and stop its shells.
    Archive {
        /// Session name.
        name: String,
    },
    /// Restore an archived session to the default list.
    Unarchive {
        /// Session name.
        name: String,
    },
    /// Delete a saved session.
    Delete {
        /// Session name.
        name: String,
        /// Stop live shells without asking.
        #[arg(long)]
        force: bool,
    },
    /// Stop a live session's shells while keeping its saved output.
    Terminate {
        /// Session name.
        name: String,
    },
    /// Print daemon statistics.
    Stats {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop the per-user daemon (all live sessions are snapshotted first).
    Shutdown,
    /// Inspect or create the configuration file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Internal: run the daemon in the foreground.
    #[command(hide = true)]
    Daemon,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Parse and validate the configuration.
    Validate {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a documented default configuration.
    Init {
        /// Replace an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Print the configuration path in use.
    Path,
}

/// Entry point. The returned code is passed to `std::process::exit`.
pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let paths = Paths::resolve(cli.config.clone(), cli.data_dir.clone())?;
    let config_override = cli.config.clone();

    match cli.command {
        Some(Commands::Daemon) => {
            paths.ensure()?;
            startup::run_daemon(paths)?;
            Ok(0)
        }
        Some(Commands::Config { action }) => {
            run_config(&paths, cli.config.is_some(), action)?;
            Ok(0)
        }
        Some(Commands::Doctor { json }) => {
            let healthy = doctor::run(&paths, json)?;
            Ok(if healthy { 0 } else { 1 })
        }
        Some(Commands::Resume(args)) => {
            launch_resume(
                &paths,
                config_override.as_deref(),
                args.session,
                args.grid,
                args.profile,
                args.cwd,
            )?;
            Ok(0)
        }
        Some(Commands::Attach { name }) => {
            launch_attach(&paths, &name)?;
            Ok(0)
        }
        Some(Commands::Sessions { all, json }) => {
            run_sessions(&paths, all, json)?;
            Ok(0)
        }
        Some(Commands::Detach { session }) => {
            let (_, mut client) = startup::existing_daemon(&paths)?;
            request_ack(
                &mut client,
                Command::DetachClients {
                    name: session.clone(),
                },
            )?;
            match session {
                Some(name) => println!("Detached FACT clients from '{name}'."),
                None => println!("Detached all FACT clients."),
            }
            Ok(0)
        }
        Some(Commands::Rename { name, new_name }) => {
            run_rename(&paths, &name, &new_name)?;
            Ok(0)
        }
        Some(Commands::Save { name }) => {
            let (_, mut client) = startup::existing_daemon(&paths)?;
            let name = match name {
                Some(name) => name,
                None => most_recent_live(&mut client)?,
            };
            request_ack(&mut client, Command::SaveSession { name: name.clone() })?;
            println!("Saved session '{name}'.");
            Ok(0)
        }
        Some(Commands::Export {
            name,
            output,
            stdout,
        }) => {
            run_export(&paths, name, output.as_deref(), stdout)?;
            Ok(0)
        }
        Some(Commands::Import {
            file,
            name,
            overwrite,
        }) => {
            run_import(&paths, &file, name, overwrite)?;
            Ok(0)
        }
        Some(Commands::Archive { name }) => {
            run_archive(&paths, &name, true)?;
            Ok(0)
        }
        Some(Commands::Unarchive { name }) => {
            run_archive(&paths, &name, false)?;
            Ok(0)
        }
        Some(Commands::Delete { name, force }) => {
            run_delete(&paths, &name, force)?;
            Ok(0)
        }
        Some(Commands::Terminate { name }) => {
            let (_, mut client) = startup::existing_daemon(&paths)?;
            request_ack(
                &mut client,
                Command::TerminateSession { name: name.clone() },
            )?;
            println!("Terminated live shells in '{name}'. The saved output is still resumable.");
            Ok(0)
        }
        Some(Commands::Stats { json }) => {
            run_stats(&paths, json)?;
            Ok(0)
        }
        Some(Commands::Shutdown) => {
            let (_, mut client) = startup::existing_daemon(&paths)?;
            request_ack(&mut client, Command::ShutdownDaemon)?;
            println!("FACT daemon stopped.");
            Ok(0)
        }
        None => {
            launch_resume(
                &paths,
                config_override.as_deref(),
                cli.launch.session,
                cli.launch.grid,
                cli.launch.profile,
                cli.launch.cwd,
            )?;
            Ok(0)
        }
    }
}

fn launch_resume(
    paths: &Paths,
    config_override: Option<&Path>,
    session: Option<String>,
    grid: u8,
    profile: Option<String>,
    cwd: Option<String>,
) -> Result<()> {
    let config = Config::load(paths)?;
    let (_, mut client) =
        startup::ensure_daemon(paths, config_override.map(Path::to_path_buf).as_ref())?;
    let cwd = cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    });
    let info = attached(
        &mut client,
        Command::ResumeSession {
            name: session,
            grid: Some(grid),
            profile,
            cwd,
        },
    )?;
    fact_client::run(client, info, config).context("terminal user interface stopped with an error")
}

fn launch_attach(paths: &Paths, name: &str) -> Result<()> {
    let config = Config::load(paths)?;
    let (_, mut client) = startup::existing_daemon(paths)?;
    let info = attached(
        &mut client,
        Command::Attach {
            name: name.to_string(),
        },
    )?;
    fact_client::run(client, info, config).context("terminal user interface stopped with an error")
}

fn attached(client: &mut DaemonClient, command: Command) -> Result<AttachInfo> {
    match client.request(command)? {
        ResponsePayload::Attached(info) => Ok(*info),
        other => bail!("unexpected daemon reply: {other:?}"),
    }
}

fn request_ack(client: &mut DaemonClient, command: Command) -> Result<()> {
    match client.request(command)? {
        ResponsePayload::Ack => Ok(()),
        other => bail!("unexpected daemon reply: {other:?}"),
    }
}

fn run_config(paths: &Paths, explicit: bool, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Path => println!("{}", paths.config_file.display()),
        ConfigAction::Validate { json } => {
            if explicit && !paths.config_file.is_file() {
                bail!(
                    "configuration file does not exist: {}",
                    paths.config_file.display()
                );
            }
            let config = Config::load(paths)?;
            let keybinds = config.compiled_keybinds()?;
            let profile = config.resolve_profile(None);
            if json {
                let value = serde_json::json!({
                    "valid": true,
                    "path": paths.config_file,
                    "built_in_defaults": !paths.config_file.is_file(),
                    "version": config.config_version,
                    "prefix": config.prefix,
                    "theme": config.theme,
                    "keybind_count": keybinds.by_action.len(),
                    "profiles": config.profile_names(),
                    "default_shell": profile.as_ref().ok().map(|p| p.command.clone()),
                    "scrollback_lines": config.scrollback_lines,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Configuration is valid: {}", paths.config_file.display());
                if !paths.config_file.is_file() {
                    println!("  (file not present; built-in defaults are in use)");
                }
                println!("  version      {}", config.config_version);
                println!("  prefix       {}", config.prefix);
                println!("  theme        {}", config.theme);
                println!("  keybindings  {} actions", keybinds.by_action.len());
                println!("  profiles     {}", config.profile_names().join(", "));
                match profile {
                    Ok(profile) => println!("  default shell {}", profile.command),
                    Err(error) => println!("  default shell <unresolved: {error}>"),
                }
            }
        }
        ConfigAction::Init { force } => {
            if paths.config_file.exists() && !force {
                bail!(
                    "{} already exists (use `fact config init --force` to replace it)",
                    paths.config_file.display()
                );
            }
            if let Some(parent) = paths.config_file.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("could not create config directory {}", parent.display())
                })?;
            }
            std::fs::write(&paths.config_file, DEFAULT_CONFIG).with_context(|| {
                format!(
                    "could not write configuration {}",
                    paths.config_file.display()
                )
            })?;
            Config::load_file(&paths.config_file)
                .context("the generated configuration failed validation")?;
            println!("Wrote {}", paths.config_file.display());
        }
    }
    Ok(())
}

fn run_sessions(paths: &Paths, all: bool, json: bool) -> Result<()> {
    let sessions = match startup::optional_daemon(paths)? {
        Some((_, mut client)) => match client.request(Command::ListSessions {
            include_archived: all,
        })? {
            ResponsePayload::Sessions(sessions) => sessions,
            other => bail!("unexpected daemon reply: {other:?}"),
        },
        None => offline_sessions(paths, all)?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!("No sessions. Run `fact` to create one.");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:>5} {:>5}  {:<20} NOTE",
        "NAME", "STATE", "PANES", "TABS", "UPDATED"
    );
    for session in sessions {
        let state = if session.live {
            "live"
        } else if session.archived {
            "archived"
        } else {
            "saved"
        };
        let note = session
            .cold_recovered_at_ms
            .map(|_| "cold-recovered")
            .unwrap_or("");
        println!(
            "{:<24} {:<10} {:>5} {:>5}  {:<20} {}",
            output::shorten(&session.name, 24),
            state,
            session.pane_count,
            session.tab_count,
            output::format_time(session.updated_at_ms),
            note
        );
    }
    Ok(())
}

fn offline_sessions(paths: &Paths, all: bool) -> Result<Vec<SessionSummary>> {
    if !paths.db_file.is_file() {
        return Ok(Vec::new());
    }
    let store = fact_session::SessionStore::open(&paths.db_file)
        .context("could not open the session database")?;
    let sessions = store.list(all).context("could not list saved sessions")?;
    Ok(sessions
        .into_iter()
        .map(|session| session.to_summary(false))
        .collect())
}

fn run_stats(paths: &Paths, json: bool) -> Result<()> {
    let (_, mut client) = startup::existing_daemon(paths)?;
    let stats = match client.request(Command::Stats)? {
        ResponsePayload::Stats(stats) => stats,
        other => bail!("unexpected daemon reply: {other:?}"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print_stats(&stats);
    }
    Ok(())
}

fn print_stats(stats: &DaemonStats) {
    println!(
        "FACT daemon {} (protocol {})",
        stats.version, stats.protocol
    );
    println!("  pid          {}", stats.daemon_pid);
    println!(
        "  uptime       {}",
        output::format_duration(stats.uptime_ms)
    );
    println!("  sessions     {}", stats.live_sessions);
    println!("  panes        {}", stats.live_panes);
    println!("  clients      {}", stats.client_count);
    println!("  database     {}", stats.database_path);
    if let Some(bytes) = stats.memory_bytes {
        println!("  memory       {}", output::format_bytes(bytes));
    }
    if let Some(ms) = stats.cpu_time_ms {
        println!("  cpu time     {}", output::format_duration(ms));
    }
}

fn most_recent_live(client: &mut DaemonClient) -> Result<String> {
    let sessions = match client.request(Command::ListSessions {
        include_archived: false,
    })? {
        ResponsePayload::Sessions(sessions) => sessions,
        other => bail!("unexpected daemon reply: {other:?}"),
    };
    sessions
        .into_iter()
        .find(|session| session.live)
        .map(|session| session.name)
        .ok_or_else(|| {
            anyhow::anyhow!("no live session; start one with `fact resume --session NAME`")
        })
}

fn latest_session_name(client: &mut DaemonClient, include_archived: bool) -> Result<String> {
    let sessions = match client.request(Command::ListSessions { include_archived })? {
        ResponsePayload::Sessions(sessions) => sessions,
        other => bail!("unexpected daemon reply: {other:?}"),
    };
    sessions
        .into_iter()
        .next()
        .map(|session| session.name)
        .ok_or_else(|| anyhow::anyhow!("no sessions to export"))
}

fn run_rename(paths: &Paths, name: &str, new_name: &str) -> Result<()> {
    let summary = if let Some((_, mut client)) = startup::optional_daemon(paths)? {
        match client.request(Command::RenameSession {
            name: name.to_string(),
            new_name: new_name.to_string(),
        })? {
            ResponsePayload::Session(summary) => summary,
            other => bail!("unexpected daemon reply: {other:?}"),
        }
    } else {
        let mut store = fact_session::SessionStore::open(&paths.db_file)
            .context("could not open the session database")?;
        store
            .rename(name, new_name)
            .context("could not rename the saved session")?;
        store
            .load(new_name)
            .context("could not read the renamed session")?
            .summary(false)
    };
    println!(
        "Renamed to '{}' ({}).",
        summary.name,
        if summary.live { "live" } else { "saved" }
    );
    Ok(())
}

fn run_archive(paths: &Paths, name: &str, archived: bool) -> Result<()> {
    let summary = if let Some((_, mut client)) = startup::optional_daemon(paths)? {
        let command = if archived {
            Command::ArchiveSession {
                name: name.to_string(),
            }
        } else {
            Command::UnarchiveSession {
                name: name.to_string(),
            }
        };
        match client.request(command)? {
            ResponsePayload::Session(summary) => summary,
            other => bail!("unexpected daemon reply: {other:?}"),
        }
    } else {
        let mut store = fact_session::SessionStore::open(&paths.db_file)
            .context("could not open the session database")?;
        store
            .set_archived(name, archived)
            .context("could not update the saved session")?;
        store
            .load(name)
            .context("could not read the saved session")?
            .summary(false)
    };
    if archived {
        println!("Archived '{}'.", summary.name);
    } else {
        println!("Restored '{}'.", summary.name);
    }
    Ok(())
}

fn run_delete(paths: &Paths, name: &str, force: bool) -> Result<()> {
    if let Some((_, mut client)) = startup::optional_daemon(paths)? {
        request_ack(
            &mut client,
            Command::DeleteSession {
                name: name.to_string(),
                force,
            },
        )?;
    } else {
        let mut store = fact_session::SessionStore::open(&paths.db_file)
            .context("could not open the session database")?;
        store
            .delete(name)
            .context("could not delete the saved session")?;
    }
    println!("Deleted '{name}'.");
    Ok(())
}

fn run_export(
    paths: &Paths,
    name: Option<String>,
    output: Option<&Path>,
    stdout: bool,
) -> Result<()> {
    let (filename, json, exported_name) =
        if let Some((_, mut client)) = startup::optional_daemon(paths)? {
            let name = match name {
                Some(name) => name,
                None => latest_session_name(&mut client, true)?,
            };
            match client.request(Command::ExportSession { name: name.clone() })? {
                ResponsePayload::Export { filename, json } => (filename, json, name),
                other => bail!("unexpected daemon reply: {other:?}"),
            }
        } else {
            let store = fact_session::SessionStore::open(&paths.db_file)
                .context("could not open the session database")?;
            let name = match name {
                Some(name) => name,
                None => store
                    .list(true)
                    .context("could not list saved sessions")?
                    .into_iter()
                    .next()
                    .map(|session| session.name)
                    .ok_or_else(|| anyhow::anyhow!("no sessions to export"))?,
            };
            let (filename, json) = store
                .export_json(&name)
                .context("could not export the saved session")?;
            (filename, json, name)
        };

    if stdout || output == Some(Path::new("-")) {
        let mut out = std::io::stdout().lock();
        out.write_all(json.as_bytes())?;
        out.write_all(b"\n")?;
        return Ok(());
    }

    let destination = match output {
        Some(path) if path.is_dir() => path.join(&filename),
        Some(path) => path.to_path_buf(),
        None => {
            paths.ensure_exports()?;
            paths.exports_dir.join(&filename)
        }
    };
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create export directory {}", parent.display()))?;
    }
    std::fs::write(&destination, json)
        .with_context(|| format!("could not write {}", destination.display()))?;
    println!("Exported '{}' to {}", exported_name, destination.display());
    Ok(())
}

fn run_import(paths: &Paths, file: &Path, name: Option<String>, overwrite: bool) -> Result<()> {
    let metadata =
        std::fs::metadata(file).with_context(|| format!("could not read {}", file.display()))?;
    if metadata.len() > 32 * 1024 * 1024 {
        bail!("import file is larger than 32 MiB");
    }
    let json = std::fs::read_to_string(file)
        .with_context(|| format!("could not read {}", file.display()))?;

    let summary = if let Some((_, mut client)) = startup::optional_daemon(paths)? {
        match client.request(Command::ImportSession {
            json,
            name,
            overwrite,
        })? {
            ResponsePayload::Imported(summary) => summary,
            other => bail!("unexpected daemon reply: {other:?}"),
        }
    } else {
        let mut store = fact_session::SessionStore::open(&paths.db_file)
            .context("could not open the session database")?;
        store
            .import_json(&json, name.as_deref(), overwrite)
            .context("could not import the session")?
            .summary(false)
    };
    println!(
        "Imported '{}' ({} panes, saved).",
        summary.name, summary.pane_count
    );
    Ok(())
}
