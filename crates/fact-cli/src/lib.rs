//! FACT command line interface.
//!
//! FACT is a single-process Windows terminal workspace. The CLI reads an
//! optional configuration file, starts the in-process engine and hands the
//! terminal to the TUI. No service, daemon or session database is involved.

/// `println!` that tolerates a closed pipe (`fact doctor | Select -First 3`)
/// instead of panicking, but still fails on any other write error.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(error) = writeln!(std::io::stdout().lock(), $($arg)*) {
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("fact: could not write output: {error}");
                std::process::exit(1);
            }
        }
    }};
}

mod doctor;
mod wallpaper;

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
    /// Let the host terminal's background image show through (see `fact wallpaper`).
    #[arg(long)]
    transparent: bool,
    /// Start in this tab. Otherwise, inside Windows Terminal with a picture
    /// wallpaper, `fact` opens a FACT tab where the picture is full resolution.
    #[arg(long)]
    here: bool,
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
    /// Show, set or clear a background image behind the workspace.
    Wallpaper {
        #[command(subcommand)]
        action: Option<WallpaperAction>,
    },
    /// Check this machine for anything that affects FACT: terminal, config,
    /// duplicate installs on PATH, agent CLIs and old data.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum WallpaperAction {
    /// Use an image (png, jpg, jpeg, gif, bmp, webp) or a video (mp4, mkv, webm,
    /// mov, avi, m4v, wmv, flv; needs ffmpeg) as the workspace background.
    Set {
        /// Image or video file. Images are copied; videos play from where they are.
        image: PathBuf,
        /// Image strength: 5-100 (percent) or 0.05-1.0. Lower keeps text easier to read.
        #[arg(long, value_parser = wallpaper::parse_opacity, default_value = "35")]
        opacity: f64,
        /// How the image fills the window.
        #[arg(long, value_enum, default_value_t = wallpaper::ImageFit::Fill)]
        fit: wallpaper::ImageFit,
    },
    /// Remove the background image and FACT's Windows Terminal profile.
    Clear,
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

    match cli.command {
        Some(Commands::Config { action }) => {
            run_config(&paths, cli.config.is_some(), action)?;
            return Ok(0);
        }
        Some(Commands::Wallpaper { action }) => {
            run_wallpaper(&paths, action)?;
            return Ok(0);
        }
        Some(Commands::Doctor) => return Ok(doctor::run(&paths)),
        None => {}
    }

    doctor::remove_legacy_data(&paths.base);
    if !cli.here && !cli.transparent && open_wallpaper_tab(&paths, &cli) {
        out!("Opened FACT in a new Windows Terminal tab with your wallpaper at full resolution.");
        out!("(Use `fact --here` to start in this tab instead.)");
        return Ok(0);
    }
    launch(&paths, cli.grid, cli.profile, cli.cwd, cli.transparent)?;
    Ok(0)
}

/// FACT can only paint a wallpaper in character cells, which looks soft.
/// Windows Terminal draws a picture at full resolution in FACT's own profile,
/// so a `fact` typed in another Windows Terminal tab opens a FACT tab there.
/// Videos are always painted by FACT (Windows Terminal cannot play them).
/// Returns `false`, and FACT starts in place, whenever that is not possible.
fn open_wallpaper_tab(paths: &Paths, cli: &Cli) -> bool {
    if !fact_client::in_windows_terminal()
        || std::env::var_os("FACT").is_some()
        || wallpaper::in_fact_profile()
    {
        return false;
    }
    let Ok(locations) = wallpaper::Locations::resolve(&paths.base) else {
        return false;
    };
    let picture = wallpaper::current(&locations)
        .ok()
        .flatten()
        .is_some_and(|current| !current.video && current.image.is_file());
    // The profile only exists while FACT's fragment is installed, and only
    // once Windows Terminal has loaded it.
    if !picture || !locations.fragment.is_file() || !wallpaper::terminal_knows_profile() {
        return false;
    }
    // A broken config is reported by starting here, not in a tab that closes.
    match Config::load(paths) {
        Ok(config) if !config.transparent_background() => {}
        _ => return false,
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // The new tab starts elsewhere, so relative paths are made absolute here.
    // `~` and `%VAR%` are left for FACT to expand.
    let here = std::env::current_dir().ok();
    let absolute = |path: &str| -> PathBuf {
        match &here {
            Some(here) if !path.starts_with('~') && !path.contains('%') => here.join(path),
            _ => PathBuf::from(path),
        }
    };
    let mut fact_args: Vec<std::ffi::OsString> = Vec::new();
    if let Some(grid) = cli.grid {
        fact_args.extend(["--grid".into(), grid.to_string().into()]);
    }
    if let Some(profile) = &cli.profile {
        fact_args.extend(["--profile".into(), profile.into()]);
    }
    if let Some(cwd) = &cli.cwd {
        fact_args.extend(["--cwd".into(), absolute(cwd).into_os_string()]);
    }
    if let Some(config) = &cli.config {
        let config = here
            .as_ref()
            .map_or_else(|| config.clone(), |here| here.join(config));
        fact_args.extend(["--config".into(), config.into_os_string()]);
    }
    let dir = match &cli.cwd {
        Some(cwd) if !cwd.starts_with('~') && !cwd.contains('%') => Some(absolute(cwd)),
        Some(_) => None,
        None => here.clone(),
    };
    let Some(args) = wallpaper::new_tab_arguments(&exe, dir.as_deref(), &fact_args) else {
        return false;
    };
    // Windows Terminal's own alias first; PATH only as a fallback (never the
    // current folder).
    let wt = std::env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join(r"Microsoft\WindowsApps\wt.exe"))
        .filter(|alias| alias.exists())
        .or_else(|| fact_config::find_on_path("wt.exe").map(PathBuf::from));
    let Some(wt) = wt else {
        return false;
    };
    std::process::Command::new(wt).args(args).spawn().is_ok()
}

fn launch(
    paths: &Paths,
    grid: Option<u8>,
    profile: Option<String>,
    cwd: Option<String>,
    transparent: bool,
) -> Result<()> {
    let mut config = Config::load(paths).context("could not load the FACT configuration")?;
    // A FACT started inside a transparent FACT pane inherits the mode, so it
    // does not paint a solid block over the outer wallpaper.
    let inherited = std::env::var("FACT_BACKGROUND").is_ok_and(|value| value == "transparent");
    let current = wallpaper::Locations::resolve(&paths.base)
        .ok()
        .and_then(|locations| wallpaper::current(&locations).ok().flatten())
        .filter(|current| current.image.is_file());
    // In Windows Terminal's FACT profile the terminal draws an image itself.
    let native = wallpaper::in_fact_profile() && current.as_ref().is_some_and(|w| !w.video);
    // `background = "transparent"` in config.toml means the same as the flag:
    // the terminal's own background shows, so FACT paints nothing.
    let leave_background = transparent || config.transparent_background() || inherited || native;
    if leave_background {
        config.background = "transparent".to_string();
    }
    let painted = if leave_background {
        None
    } else {
        current.map(|current| fact_client::WallpaperSpec {
            image: current.image,
            opacity: current.opacity as f32,
            fit: current.fit.into(),
        })
    };
    if config.transparent_background() || painted.is_some() {
        config
            .env
            .entry("FACT_BACKGROUND".to_string())
            .or_insert_with(|| "transparent".to_string());
    }
    let cwd = cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    });

    let started = fact_engine::start(config.clone(), grid.unwrap_or(1), profile, cwd)
        .context("could not start the FACT workspace")?;
    let result = fact_client::run(started.client, started.events, config, painted)
        .context("terminal user interface stopped with an error");
    drop(started.engine);
    result
}

fn run_wallpaper(paths: &Paths, action: Option<WallpaperAction>) -> Result<()> {
    let locations = wallpaper::Locations::resolve(&paths.base)?;
    match action {
        None => match wallpaper::current(&locations)? {
            Some(current) => {
                out!("Wallpaper: {}", wallpaper::describe(&current));
                out!("It shows behind FACT in every terminal.");
                if !current.video {
                    out!(
                        "Full resolution in Windows Terminal: wt -p {}",
                        wallpaper::PROFILE_NAME
                    );
                }
            }
            None => out!("No wallpaper set. Use: fact wallpaper set <image>"),
        },
        Some(WallpaperAction::Set {
            image,
            opacity,
            fit,
        }) => {
            let current = wallpaper::set(&locations, &image, opacity, fit)?;
            out!("Wallpaper set: {}", wallpaper::describe(&current));
            out!("It shows behind FACT in every terminal the next time you run `fact`.");
            wallpaper::reload_windows_terminal();
            if !current.video {
                out!(
                    "In Windows Terminal, `wt -p {}` shows it at full resolution.",
                    wallpaper::PROFILE_NAME
                );
            }
        }
        Some(WallpaperAction::Clear) => {
            if wallpaper::clear(&locations)? {
                wallpaper::reload_windows_terminal();
                out!("Wallpaper and the FACT Windows Terminal profile were removed.");
            } else {
                out!("No wallpaper was set.");
            }
        }
    }
    Ok(())
}

fn run_config(paths: &Paths, explicit: bool, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Path => out!("{}", paths.config_file.display()),
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
                    "background": config.background,
                    "alt_shortcuts": config.alt_shortcuts,
                    "keybind_count": keybinds.by_action.len(),
                    "profiles": config.profile_names(),
                    "default_shell": profile.as_ref().ok().map(|profile| profile.command.clone()),
                    "scrollback_lines": config.scrollback_lines,
                });
                out!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                out!("Configuration is valid: {}", paths.config_file.display());
                if !paths.config_file.is_file() {
                    out!("  (file not present; built-in defaults are in use)");
                }
                out!("  version      {}", config.config_version);
                out!("  prefix       {}", config.prefix);
                out!("  theme        {}", config.theme);
                out!("  background   {}", config.background);
                out!(
                    "  alt keys     {}",
                    if config.alt_shortcuts { "on" } else { "off" }
                );
                out!("  keybindings  {} actions", keybinds.by_action.len());
                out!("  profiles     {}", config.profile_names().join(", "));
                match profile {
                    Ok(profile) => out!("  default shell {}", profile.command),
                    Err(error) => out!("  default shell <unresolved: {error}>"),
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
            out!("Wrote {}", paths.config_file.display());
        }
    }
    Ok(())
}
