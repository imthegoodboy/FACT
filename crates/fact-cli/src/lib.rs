//! FACT command line interface.
//!
//! FACT is a single-process Windows terminal workspace. The CLI reads an
//! optional configuration file, starts the in-process engine and hands the
//! terminal to the TUI. No service, daemon or session database is involved.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fact_config::{Config, Paths};

#[derive(Debug, Parser)]
#[command(
    name = "fact",
    version,
    about = "FACT — a Windows-native terminal workspace",
    long_about = "FACT opens real PowerShell panes in one terminal workspace. Split, tab and \
                  resize with the keyboard or mouse. Every shell exits when FACT exits; nothing \
                  is saved.",
    propagate_version = true
)]
struct Cli {
    /// Configuration file (defaults to %LOCALAPPDATA%\FACT\config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Start a grid with this many independent shells (1-32).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=32))]
    grid: Option<u8>,
    /// Shell profile from the configuration.
    #[arg(long, value_name = "NAME")]
    profile: Option<String>,
    /// Working directory for the initial shells.
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Inspect or create the configuration file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
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

pub fn run() -> Result<i32> {
    let cli = Cli::parse();
    let paths = Paths::resolve(cli.config.clone())?;

    if let Some(Commands::Config { action }) = cli.command {
        run_config(&paths, cli.config.is_some(), action)?;
        return Ok(0);
    }

    launch(&paths, cli.grid, cli.profile, cli.cwd)?;
    Ok(0)
}

fn launch(
    paths: &Paths,
    grid: Option<u8>,
    profile: Option<String>,
    cwd: Option<String>,
) -> Result<()> {
    let config = Config::load(paths).context("could not load the FACT configuration")?;
    let cwd = cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    });

    let started = fact_engine::start(config.clone(), grid.unwrap_or(1), profile, cwd)
        .context("could not start the FACT workspace")?;
    let result = fact_client::run(started.client, started.events, config)
        .context("terminal user interface stopped with an error");
    drop(started.engine);
    result
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
                    "default_shell": profile.as_ref().ok().map(|profile| profile.command.clone()),
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
            fact_config::write_default_config(&paths.config_file, force)?;
            Config::load_file(&paths.config_file)
                .context("the generated configuration failed validation")?;
            println!("Wrote {}", paths.config_file.display());
        }
    }
    Ok(())
}
