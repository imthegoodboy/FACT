//! `fact doctor`: a read-only report of everything that can make FACT behave
//! differently on this machine — the host terminal, configuration, competing
//! `fact` installs on PATH, agent CLIs and leftovers from older versions.

use std::path::{Path, PathBuf};

use fact_config::{Config, Paths};

use crate::wallpaper;

/// Agent CLIs FACT is regularly used with, and what helps each one inside it.
const AGENTS: &[(&str, &str)] = &[
    (
        "claude",
        "Esc interrupts; /tui default keeps output in FACT scrollback",
    ),
    (
        "codex",
        "codex --no-alt-screen keeps output in FACT scrollback",
    ),
    ("gemini", "runs as a normal full-screen app"),
    (
        "opencode",
        "OTUI_USE_ALTERNATE_SCREEN=false keeps output in scrollback",
    ),
    ("copilot", "runs as a normal full-screen app"),
    ("aider", "inline app; output stays in FACT scrollback"),
    ("qwen", "runs as a normal full-screen app"),
    ("cursor-agent", "runs as a normal full-screen app"),
];

/// Files that FACT 0.3 and earlier kept for its background daemon (a session
/// database, runtime identity and logs). FACT no longer stores anything, so
/// launching FACT removes them; `fact doctor` only reports them.
const LEGACY: &[&str] = &[
    "sessions.db",
    "sessions.db-wal",
    "sessions.db-shm",
    "runtime",
    "logs",
];

pub fn run(paths: &Paths) -> i32 {
    let mut warnings = 0;
    let exe = std::env::current_exe().ok();
    out!(
        "FACT {}  {}",
        env!("CARGO_PKG_VERSION"),
        exe.as_deref().map(display).unwrap_or_default()
    );
    out!();

    row("Terminal", &terminal_host());
    if std::env::var_os("FACT").is_some() {
        row(
            "",
            "running inside a FACT pane; `fact` here starts a nested workspace",
        );
    }

    match Config::load(paths) {
        Ok(config) => {
            let source = if paths.config_file.is_file() {
                display(&paths.config_file)
            } else {
                "built-in defaults (no config.toml)".to_string()
            };
            row("Config", &format!("ok · {source}"));
            match config.resolve_profile(None) {
                Ok(profile) => row("Shell", &profile.command),
                Err(error) => {
                    warnings += 1;
                    row("Shell", &format!("! {error}"));
                }
            }
            row(
                "Keys",
                &format!(
                    "prefix {} · Alt shortcuts {}",
                    config.prefix,
                    if config.alt_shortcuts { "on" } else { "off" }
                ),
            );
            row("Background", &config.background);
        }
        Err(error) => {
            warnings += 1;
            row("Config", &format!("! {error}"));
        }
    }

    let installs = on_path("fact");
    match installs.as_slice() {
        [] => row(
            "On PATH",
            "fact is not on PATH (run it by full path or install it)",
        ),
        [only] => row("On PATH", &display(only)),
        [first, rest @ ..] => {
            warnings += 1;
            row("On PATH", &format!("{} (runs first)", display(first)));
            for other in rest {
                row("", &format!("! also {}", display(other)));
            }
            row("", "  several installs: remove the ones you do not use");
        }
    }

    out!();
    for (name, tip) in AGENTS {
        match on_path(name).first() {
            Some(path) => row(name, &format!("{} · {tip}", display(path))),
            None => continue,
        }
    }

    out!();
    match wallpaper::Locations::resolve(&paths.base).and_then(|l| wallpaper::current(&l)) {
        Ok(Some(current)) => {
            row("Wallpaper", &wallpaper::describe(&current));
            if !current.image.is_file() {
                warnings += 1;
                row(
                    "",
                    "! the file is missing; run `fact wallpaper set <file>` again",
                );
            } else if current.video && fact_client::find_ffmpeg().is_none() {
                warnings += 1;
                row("", "! video wallpapers need ffmpeg: winget install ffmpeg");
            }
        }
        Ok(None) => row("Wallpaper", "none (fact wallpaper set <image or video>)"),
        Err(error) => {
            warnings += 1;
            row(
                "Wallpaper",
                &format!("! could not read the profile: {error}"),
            );
        }
    }

    let legacy: Vec<PathBuf> = LEGACY
        .iter()
        .map(|name| paths.base.join(name))
        .filter(|path| path.exists())
        .collect();
    if !legacy.is_empty() {
        warnings += 1;
        row(
            "Old data",
            "! files from FACT 0.3 or earlier; the next `fact` launch removes them:",
        );
        for path in &legacy {
            row("", &display(path));
        }
    }

    out!();
    if warnings == 0 {
        out!("No problems found.");
    } else {
        out!(
            "{warnings} item{} attention (marked !).",
            if warnings == 1 { " needs" } else { "s need" }
        );
    }
    0
}

/// Delete the daemon-era files listed in [`LEGACY`] from FACT's folder. Only
/// those exact names are touched. Best effort: a locked file stays behind.
pub fn remove_legacy_data(base: &Path) {
    for name in LEGACY {
        let path = base.join(name);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn row(label: &str, value: &str) {
    out!("  {label:<12} {value}");
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn terminal_host() -> String {
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if program.eq_ignore_ascii_case("vscode") {
        "VS Code terminal".into()
    } else if fact_client::in_windows_terminal() {
        "Windows Terminal (full-resolution wallpaper, true color, full key reporting)".into()
    } else if !program.is_empty() {
        program
    } else {
        "classic console; Windows Terminal gives sharper wallpapers".into()
    }
}

/// Every launcher for `name` on PATH, in the order Windows resolves them.
fn on_path(name: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    // Windows tries extensions in PATHEXT order within each directory.
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect();
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = dir.join(format!("{name}.{extension}"));
            if candidate.is_file()
                && !found
                    .iter()
                    .any(|seen| seen.as_os_str().eq_ignore_ascii_case(candidate.as_os_str()))
            {
                found.push(candidate);
                // One launcher per directory: Windows runs the first match.
                break;
            }
        }
    }
    found
}
