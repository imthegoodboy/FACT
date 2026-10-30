//! Configuration, filesystem locations and shell profiles for FACT.

pub mod keys;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub use keys::{compile, CompiledKeybinds, KeyChord, KeyParseError, KeybindError};

pub const CONFIG_VERSION: u32 = 2;
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine a usable FACT configuration directory")]
    NoBaseDirectory,
    #[error("could not read config '{path}': {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write config '{path}': {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config '{path}' is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("config version {found} is not supported (expected {expected})")]
    Version { found: u32, expected: u32 },
    #[error("configuration is invalid: {0}")]
    Invalid(String),
    #[error("profile '{0}' does not exist")]
    UnknownProfile(String),
    #[error("could not resolve a shell for profile '{0}': {1}")]
    NoShell(String, String),
    #[error("config path already exists: {0}")]
    Exists(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Paths {
    /// Directory that holds the optional configuration file. FACT never writes
    /// runtime state here.
    pub base: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    pub fn resolve(config_override: Option<PathBuf>) -> Result<Self, ConfigError> {
        let base = std::env::var_os("FACT_HOME")
            .map(PathBuf::from)
            .or_else(default_base_dir)
            .ok_or(ConfigError::NoBaseDirectory)?;
        let config_file = config_override.unwrap_or_else(|| base.join(DEFAULT_CONFIG_FILE));
        Ok(Self::from_parts(base, config_file))
    }

    pub fn from_parts(base: PathBuf, config_file: PathBuf) -> Self {
        Self { base, config_file }
    }
}

fn default_base_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    if let Some(local) = local {
        return Some(local.join("FACT"));
    }
    std::env::current_dir().ok().map(|cwd| cwd.join(".fact"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub config_version: u32,
    /// Chord that arms the FACT command prefix.
    pub prefix: String,
    /// One of `fact-dark`, `fact-light`, `dracula`, `nord`, `gruvbox`.
    pub theme: String,
    pub scrollback_lines: usize,
    pub min_pane_width: u16,
    pub min_pane_height: u16,
    pub mouse: bool,
    pub confirm_close: bool,
    pub copy_on_select: bool,
    /// Direct `Alt+M` / `Alt+N` / `Alt+T` shortcuts. Off by default because
    /// agent CLIs and readline use those chords themselves.
    pub alt_shortcuts: bool,
    /// `theme` paints the theme background; `transparent` leaves default
    /// backgrounds to the host terminal so its background image shows through.
    pub background: String,
    pub default_profile: String,
    pub keybinds: BTreeMap<String, String>,
    pub profiles: BTreeMap<String, Profile>,
    /// Extra environment values applied to every pane. Values are visible to
    /// child processes; do not put secrets here.
    pub env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            prefix: "ctrl+space".to_string(),
            theme: "fact-dark".to_string(),
            scrollback_lines: 8000,
            min_pane_width: 16,
            min_pane_height: 5,
            mouse: true,
            confirm_close: true,
            copy_on_select: false,
            alt_shortcuts: false,
            background: "theme".to_string(),
            default_profile: "auto".to_string(),
            keybinds: BTreeMap::new(),
            profiles: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Executable name or absolute path. Empty/omitted auto-detects a shell.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub title: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub title: Option<String>,
}

impl ResolvedProfile {
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            Path::new(&self.command)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&self.command)
                .to_string()
        })
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self, ConfigError> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        Self::load_file(&paths.config_file)
    }

    pub fn load_file(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        if config.config_version != CONFIG_VERSION {
            return Err(ConfigError::Version {
                found: config.config_version,
                expected: CONFIG_VERSION,
            });
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut issues = Vec::new();
        if !(100..=100_000).contains(&self.scrollback_lines) {
            issues.push("scrollback_lines must be in 100..=100000".to_string());
        }
        if self.min_pane_width < 12 {
            issues.push("min_pane_width must be at least 12".to_string());
        }
        if self.min_pane_height < 3 {
            issues.push("min_pane_height must be at least 3".to_string());
        }
        if !THEMES.contains(&self.theme.as_str()) {
            issues.push(format!(
                "theme '{}' is unknown (expected fact-dark, fact-light, dracula, nord or gruvbox)",
                self.theme
            ));
        }
        if !BACKGROUNDS.contains(&self.background.as_str()) {
            issues.push(format!(
                "background '{}' is unknown (expected theme or transparent)",
                self.background
            ));
        }
        if self.default_profile.trim().is_empty() {
            issues.push("default_profile may not be empty".to_string());
        }
        if let Err(err) = keys::parse_prefix(&self.prefix) {
            issues.push(format!("prefix '{}' is invalid: {err}", self.prefix));
        }
        if let Err(err) = keys::compile(
            keys::parse_prefix(&self.prefix).unwrap_or(keys::KeyChord::new(
                keys::Modifiers::CONTROL,
                keys::KeyCode::Space,
            )),
            &self.keybinds,
        ) {
            issues.push(err.to_string());
        }
        for (name, profile) in &self.profiles {
            if name.trim().is_empty() || name.len() > 64 {
                issues.push("profile names must be 1..=64 characters".to_string());
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                issues.push(format!("profile name '{name}' contains invalid characters"));
            }
            if !profile.enabled {
                continue;
            }
            if let Some(command) = &profile.command {
                if command.trim().is_empty() {
                    issues.push(format!("profile '{name}' has an empty command"));
                }
                if command.chars().any(|c| c == '\0' || c == '\r' || c == '\n') {
                    issues.push(format!(
                        "profile '{name}' command contains control characters"
                    ));
                }
            }
            for arg in &profile.args {
                if arg.contains('\0') {
                    issues.push(format!("profile '{name}' has an argument containing NUL"));
                }
            }
            if let Some(cwd) = &profile.cwd {
                if cwd.contains('\0') {
                    issues.push(format!("profile '{name}' cwd contains NUL"));
                }
            }
            validate_env_map(&format!("profile '{name}'"), &profile.env, &mut issues);
        }
        validate_env_map("global env", &self.env, &mut issues);
        if !issues.is_empty() {
            return Err(ConfigError::Invalid(issues.join("; ")));
        }
        Ok(())
    }

    pub fn prefix_chord(&self) -> Result<KeyChord, ConfigError> {
        keys::parse_prefix(&self.prefix)
            .map_err(|e| ConfigError::Invalid(format!("prefix '{}': {e}", self.prefix)))
    }

    pub fn compiled_keybinds(&self) -> Result<CompiledKeybinds, ConfigError> {
        keys::compile(self.prefix_chord()?, &self.keybinds)
            .map_err(|e| ConfigError::Invalid(e.to_string()))
    }

    /// Resolve a profile by name. `None` uses `default_profile`.
    pub fn resolve_profile(&self, requested: Option<&str>) -> Result<ResolvedProfile, ConfigError> {
        let name = requested
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.default_profile.as_str());
        if name.eq_ignore_ascii_case("auto") {
            let command = detect_shell().ok_or_else(|| {
                ConfigError::NoShell(
                    "auto".to_string(),
                    "could not find pwsh.exe, powershell.exe or cmd.exe on PATH".to_string(),
                )
            })?;
            return Ok(ResolvedProfile {
                name: "auto".to_string(),
                command,
                args: Vec::new(),
                cwd: home_dir(),
                env: self.env.clone(),
                title: None,
            });
        }
        let profile = self
            .profiles
            .get(name)
            .ok_or_else(|| ConfigError::UnknownProfile(name.to_string()))?;
        if !profile.enabled {
            return Err(ConfigError::Invalid(format!(
                "profile '{name}' is disabled"
            )));
        }
        let command = match profile.command.as_deref().map(str::trim) {
            // `claude` or `pwsh.exe` work like at a prompt. A command that is
            // not found is passed on unchanged so the pane reports the error.
            Some(c) if !c.is_empty() => find_on_path(c).unwrap_or_else(|| c.to_string()),
            _ => detect_shell().ok_or_else(|| {
                ConfigError::NoShell(
                    name.to_string(),
                    "profile has no command and no supported shell was found".to_string(),
                )
            })?,
        };
        let mut env = self.env.clone();
        env.extend(profile.env.clone());
        Ok(ResolvedProfile {
            name: name.to_string(),
            command,
            args: profile.args.clone(),
            cwd: profile
                .cwd
                .as_deref()
                .map(expand_path)
                .transpose()
                .map_err(ConfigError::Invalid)?
                .or_else(home_dir),
            env,
            title: profile.title.clone(),
        })
    }

    pub fn transparent_background(&self) -> bool {
        self.background == "transparent"
    }

    /// The theme's terminal text and background colors, reported to programs
    /// that ask (OSC 10/11). Kept in step with fact-client's themes.
    pub fn terminal_colors(&self) -> ([u8; 3], [u8; 3]) {
        match self.theme.as_str() {
            "fact-light" => ([31, 35, 40], [255, 255, 255]),
            "dracula" => ([248, 248, 242], [33, 34, 44]),
            "nord" => ([216, 222, 233], [36, 41, 51]),
            "gruvbox" => ([235, 219, 178], [29, 32, 33]),
            _ => ([231, 233, 237], [0, 0, 0]),
        }
    }

    pub fn has_profile(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case("auto") || self.profiles.contains_key(name)
    }

    pub fn profile_names(&self) -> Vec<String> {
        let mut names = vec!["auto".to_string()];
        names.extend(self.profiles.keys().cloned());
        names
    }
}

fn validate_env_map(label: &str, env: &BTreeMap<String, String>, issues: &mut Vec<String>) {
    for (key, value) in env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            issues.push(format!("{label} has an invalid environment variable name"));
        }
        if value.contains('\0') {
            issues.push(format!("{label} environment variable '{key}' contains NUL"));
        }
    }
}

pub fn detect_shell() -> Option<String> {
    if let Some(comspec) = std::env::var_os("COMSPEC") {
        // Prefer PowerShell 7, then Windows PowerShell, then CMD.
        if let Some(pwsh) = find_on_path("pwsh.exe") {
            return Some(pwsh);
        }
        if let Some(ps) = find_on_path("powershell.exe") {
            return Some(ps);
        }
        return Some(PathBuf::from(comspec).to_string_lossy().into_owned());
    }
    find_on_path("pwsh.exe")
        .or_else(|| find_on_path("powershell.exe"))
        .or_else(|| find_on_path("cmd.exe"))
}

/// Find a program much like the command prompt does: a name with a folder is
/// used as given, a bare name is looked up in PATH's absolute folders, and a
/// name without a program extension tries each PATHEXT extension (`claude`
/// finds `claude.cmd`). Unlike the prompt, the current folder is never
/// searched.
pub fn find_on_path(exe: &str) -> Option<String> {
    let name = Path::new(exe);
    let pathext: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| ext.starts_with('.') && ext.len() > 1)
        .collect();
    let has_program_extension = name
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| pathext.contains(&format!(".{}", ext.to_ascii_lowercase())));
    let extensions: Vec<&str> = if has_program_extension {
        vec![""]
    } else {
        pathext.iter().map(String::as_str).collect()
    };
    let first_match = |base: &Path| {
        extensions.iter().find_map(|ext| {
            let mut file = base.as_os_str().to_os_string();
            file.push(ext);
            let file = PathBuf::from(file);
            file.is_file().then(|| file.to_string_lossy().into_owned())
        })
    };
    if name.is_absolute() || name.components().count() > 1 {
        return first_match(name);
    }
    // Relative PATH entries (`.`, empty) would mean the current folder.
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| first_match(&dir.join(name)))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut result = drive;
            result.push(path);
            Some(PathBuf::from(result))
        })
}

/// Expand `~`, `%VAR%` and `$env:VAR`-style references used in profile cwd values.
pub fn expand_path(input: &str) -> Result<PathBuf, String> {
    if input.contains('\0') {
        return Err("path contains NUL".to_string());
    }
    let mut expanded = if let Some(rest) = input.strip_prefix("~") {
        let home = home_dir().ok_or_else(|| "could not determine home directory".to_string())?;
        let rest = rest.trim_start_matches(['/', '\\']);
        if rest.is_empty() {
            home
        } else {
            home.join(rest)
        }
    } else {
        PathBuf::from(input)
    };
    let mut text = expanded.to_string_lossy().into_owned();
    let mut start = 0;
    while let Some(open) = text[start..].find('%') {
        let open = start + open;
        let Some(close_rel) = text[open + 1..].find('%') else {
            break;
        };
        let close = open + 1 + close_rel;
        let var = &text[open + 1..close];
        if var.is_empty() || var.len() > 128 {
            start = close + 1;
            continue;
        }
        if let Some(value) = std::env::var_os(var) {
            let value = value.to_string_lossy();
            text.replace_range(open..=close, &value);
            start = open + value.len();
        } else {
            start = close + 1;
        }
    }
    expanded = PathBuf::from(text);
    Ok(expanded)
}

pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# FACT — Fast, Advanced, Reliable Terminal Workspace
# Configuration reference: https://github.com/imthegoodboy/FACT/blob/main/docs/configuration.md
config_version = 2

# Prefix chord arms FACT commands. Press it, then a command key (prefix+k = palette).
prefix = "ctrl+space"
theme = "fact-dark"

scrollback_lines = 8000
min_pane_width = 16
min_pane_height = 5
mouse = true
confirm_close = true
copy_on_select = false

# Alt+M / Alt+N / Alt+T maximize, show all and close the tab without the
# prefix. Off by default: Claude Code, readline and other apps use these chords.
alt_shortcuts = false

# "theme" paints the theme background. "transparent" lets the host terminal's
# background image or acrylic show through (see `fact wallpaper`).
background = "theme"

# "auto" chooses pwsh.exe, then powershell.exe, then cmd.exe.
default_profile = "auto"

[keybinds]
# Override any documented action, or set it to "none" to free the key, e.g.:
# split_right = "prefix+\\"
# palette = "prefix+k"
# help = "none"

# [profiles.pwsh]
# command = "pwsh.exe"
# args = ["-NoLogo"]
# cwd = "~"
#
# [profiles.wsl]
# command = "wsl.exe"
# args = ["-d", "Ubuntu", "--cd", "~"]
#
# [profiles.cmd]
# command = "cmd.exe"

[env]
# FACT sets FACT, FACT_PANE, FACT_VERSION and (if unset) COLORTERM itself.
"#;

pub fn write_default_config(path: &Path, force: bool) -> Result<(), ConfigError> {
    if path.exists() && !force {
        return Err(ConfigError::Exists(path.to_path_buf()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let temp = path.with_extension("toml.tmp");
    {
        let mut file = fs::File::create(&temp).map_err(|source| ConfigError::Write {
            path: temp.clone(),
            source,
        })?;
        file.write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|source| ConfigError::Write {
                path: temp.clone(),
                source,
            })?;
    }
    fs::rename(&temp, path).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Accepted `background` values.
pub const BACKGROUNDS: &[&str] = &["theme", "transparent"];

/// Stable theme names accepted by the client.
pub const THEMES: &[&str] = &["fact-dark", "fact-light", "dracula", "nord", "gruvbox"];

pub fn is_valid_theme(name: &str) -> bool {
    THEMES.contains(&name)
}

/// A profile's environment merged with FACT's own values.
///
/// `TERM` and `TERM_PROGRAM` are deliberately inherited, not replaced. Agent
/// CLIs pick their keyboard protocol from them: Claude Code turns on the kitty
/// protocol at once inside Windows Terminal, which FACT now understands, but
/// falls back to a fragile timing probe for an unknown `TERM_PROGRAM`.
pub fn pane_environment(profile: &ResolvedProfile) -> BTreeMap<String, String> {
    let mut env = profile.env.clone();
    env.entry("COLORTERM".to_string())
        .or_insert_with(|| "truecolor".to_string());
    env.entry("FACT".to_string())
        .or_insert_with(|| "1".to_string());
    env.entry("FACT_VERSION".to_string())
        .or_insert_with(|| env!("CARGO_PKG_VERSION").to_string());
    env
}

pub fn warning_env_names(env: &BTreeMap<String, String>) -> HashSet<String> {
    env.keys().map(|k| k.to_ascii_uppercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().unwrap();
        assert!(Config::default().compiled_keybinds().is_ok());
    }

    #[test]
    fn bad_config_values_fail() {
        let config = Config {
            scrollback_lines: 3,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        let config = Config {
            theme: "neon".to_string(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
        let config = Config {
            prefix: "ctrl+".to_string(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn background_must_be_known() {
        let config = Config {
            background: "image".to_string(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
        let config = Config {
            background: "transparent".to_string(),
            ..Config::default()
        };
        config.validate().unwrap();
        assert!(config.transparent_background());
        assert!(!Config::default().transparent_background());
        assert!(!Config::default().alt_shortcuts);
    }

    #[test]
    fn pane_environment_marks_fact_and_keeps_the_host_terminal() {
        let mut profile = Config::default().resolve_profile(None).unwrap();
        profile
            .env
            .insert("COLORTERM".to_string(), "24bit".to_string());
        let env = pane_environment(&profile);
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("24bit"));
        assert_eq!(env.get("FACT").map(String::as_str), Some("1"));
        assert!(!env.contains_key("TERM"));
        assert!(!env.contains_key("TERM_PROGRAM"));
    }

    #[test]
    fn obsolete_daemon_settings_are_rejected() {
        let parsed: Result<Config, _> = toml::from_str("config_version = 2\nmax_sessions = 4\n");
        assert!(parsed.is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "config_version = 2\nnot_a_setting = true\n").unwrap();
        assert!(Config::load_file(&path).is_err());
    }

    #[test]
    fn env_expansion_and_profile_resolution() {
        std::env::set_var("FACT_TEST_EXPAND", "expanded");
        assert_eq!(
            expand_path("%FACT_TEST_EXPAND%\\child").unwrap(),
            PathBuf::from("expanded\\child")
        );
        let mut config = Config::default();
        config.profiles.insert(
            "test".to_string(),
            Profile {
                command: Some("cmd.exe".to_string()),
                args: vec!["/K".to_string(), "echo hi".to_string()],
                cwd: None,
                env: BTreeMap::new(),
                title: Some("Test".to_string()),
                enabled: true,
            },
        );
        let p = config.resolve_profile(Some("test")).unwrap();
        // Found on PATH, so the pane can start it.
        assert!(Path::new(&p.command).is_absolute(), "{}", p.command);
        assert!(p.command.to_ascii_lowercase().ends_with("cmd.exe"));
        assert_eq!(p.label(), "Test");
        assert!(config.resolve_profile(Some("missing")).is_err());
    }

    #[test]
    fn commands_without_extension_find_batch_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tool.cmd"), "@echo off\n").unwrap();
        let bare = dir.path().join("tool");
        let found = find_on_path(&bare.to_string_lossy()).unwrap();
        assert!(found.to_ascii_lowercase().ends_with("tool.cmd"), "{found}");
        assert!(find_on_path(&dir.path().join("missing").to_string_lossy()).is_none());
        assert!(find_on_path("fact-surely-not-installed-anywhere").is_none());
    }

    #[test]
    fn default_config_round_trips() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TEMPLATE).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.default_profile, "auto");
    }

    #[test]
    fn disabled_profile_rejected() {
        let mut config = Config::default();
        let profile = Profile {
            enabled: false,
            ..Profile::default()
        };
        config.profiles.insert("x".into(), profile);
        assert!(config.resolve_profile(Some("x")).is_err());
    }
}
