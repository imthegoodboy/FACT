//! Human-readable key chords used by the configuration and the TUI.
//!
//! This module deliberately has no dependency on crossterm so configuration can
//! be validated, tested and reused without a console.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        control: false,
        alt: false,
        shift: false,
    };
    pub const CONTROL: Self = Self {
        control: true,
        alt: false,
        shift: false,
    };
    pub const SHIFT: Self = Self {
        control: false,
        alt: false,
        shift: true,
    };

    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.control {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.join("+")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    F(u8),
    Enter,
    Tab,
    Esc,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Space,
}

impl KeyCode {
    pub fn label(self) -> String {
        match self {
            KeyCode::Char(' ') => "Space".into(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Space => "Space".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyChord {
    pub mods: Modifiers,
    pub code: KeyCode,
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self {
            control: self.control || rhs.control,
            alt: self.alt || rhs.alt,
            shift: self.shift || rhs.shift,
        }
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mods = self.mods.label();
        if mods.is_empty() {
            write!(f, "{}", self.code.label())
        } else {
            write!(f, "{}+{}", mods, self.code.label())
        }
    }
}

impl KeyChord {
    pub fn new(mods: Modifiers, code: KeyCode) -> Self {
        Self { mods, code }
    }

    pub fn is_modifier_only(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Binding {
    pub prefix: bool,
    pub chord: KeyChord,
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix {
            write!(f, "prefix+{}", self.chord)
        } else {
            write!(f, "{}", self.chord)
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyParseError {
    #[error("key binding is empty")]
    Empty,
    #[error("unknown modifier '{0}'")]
    UnknownModifier(String),
    #[error("missing key after modifier")]
    MissingKey,
    #[error("key binding must contain exactly one key")]
    MultipleKeys,
    #[error("unknown key name '{0}'")]
    UnknownKey(String),
    #[error("function key must be between F1 and F24")]
    FunctionKeyRange,
    #[error("prefix may not be combined with itself")]
    RecursivePrefix,
}

/// Parse a chord such as `Ctrl+Shift+C`, `F1`, `prefix+?` or `prefix+Shift+Left`.
pub fn parse_binding(input: &str) -> Result<Binding, KeyParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(KeyParseError::Empty);
    }
    let mut parts = split_binding(input)?;
    let prefix = parts
        .first()
        .is_some_and(|p| p.eq_ignore_ascii_case("prefix"));
    if prefix {
        parts.remove(0);
        if parts.is_empty() {
            return Err(KeyParseError::MissingKey);
        }
        if parts.iter().any(|p| p.eq_ignore_ascii_case("prefix")) {
            return Err(KeyParseError::RecursivePrefix);
        }
    }
    let mut mods = Modifiers::NONE;
    let mut code = None;
    for (idx, part) in parts.iter().enumerate() {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods.control = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => {
                if idx + 1 != parts.len() {
                    return Err(KeyParseError::MultipleKeys);
                }
                code = Some(parse_key(part)?);
            }
        }
    }
    let code = code.ok_or(KeyParseError::MissingKey)?;
    // `Ctrl+Shift+A` and `Ctrl+A` with an uppercase key are equivalent for
    // crossterm, but we keep the explicit modifier and normalize the key to
    // lowercase so bindings compare consistently with terminal events.
    let code = match code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            mods.shift = true;
            KeyCode::Char(c.to_ascii_lowercase())
        }
        other => other,
    };
    Ok(Binding {
        prefix,
        chord: KeyChord { mods, code },
    })
}

/// Split a binding into modifier/key tokens.
///
/// `+` doubles as the separator and as the literal plus key, so a trailing
/// `++` is read as "literal plus" (`prefix++`, `ctrl+shift++`). A lone `+`
/// refers to the plus key itself.
fn split_binding(input: &str) -> Result<Vec<String>, KeyParseError> {
    if input == "+" {
        return Ok(vec!["+".to_string()]);
    }
    let mut body = input;
    let literal_plus = input.ends_with("++");
    if literal_plus {
        // Keep one trailing `+` as the separator for the literal key token.
        body = &input[..input.len() - 1];
    }
    let mut parts: Vec<String> = body.split('+').map(|p| p.trim().to_string()).collect();
    if literal_plus {
        if parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        parts.push("+".to_string());
    }
    if parts.iter().any(|p| p.is_empty()) {
        return Err(KeyParseError::MissingKey);
    }
    Ok(parts)
}

fn parse_key(part: &str) -> Result<KeyCode, KeyParseError> {
    let lower = part.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "space" => KeyCode::Space,
        _ if lower.len() >= 2
            && (lower.starts_with('f') || lower.starts_with('F'))
            && lower[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            let n: u8 = lower[1..]
                .parse()
                .map_err(|_| KeyParseError::UnknownKey(part.to_string()))?;
            if !(1..=24).contains(&n) {
                return Err(KeyParseError::FunctionKeyRange);
            }
            KeyCode::F(n)
        }
        _ => {
            let mut chars = part.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return Err(KeyParseError::UnknownKey(part.to_string())),
            }
        }
    };
    Ok(code)
}

pub fn parse_prefix(input: &str) -> Result<KeyChord, KeyParseError> {
    let binding = parse_binding(input)?;
    if binding.prefix {
        return Err(KeyParseError::RecursivePrefix);
    }
    Ok(binding.chord)
}

pub const ACTIONS: &[&str] = &[
    "split_right",
    "split_down",
    "close_pane",
    "close_tab",
    "restart_pane",
    "zoom",
    "maximize",
    "show_all",
    "minimize",
    "restore",
    "focus_left",
    "focus_right",
    "focus_up",
    "focus_down",
    "next_pane",
    "prev_pane",
    "new_tab",
    "next_tab",
    "prev_tab",
    "rename_tab",
    "palette",
    "search",
    "scroll_mode",
    "help",
    "copy",
    "paste",
    "quit",
    "swap_pane",
    "move_pane",
    "resize_left",
    "resize_right",
    "resize_up",
    "resize_down",
    "balance",
    "cycle_theme",
];

/// Binding value that leaves an action without a key.
pub const UNBOUND: &str = "none";

pub fn default_keybinds() -> BTreeMap<String, String> {
    [
        ("split_right", "prefix+|"),
        ("split_down", "prefix+-"),
        ("close_pane", "prefix+x"),
        ("close_tab", "prefix+shift+x"),
        ("restart_pane", "prefix+shift+r"),
        ("zoom", "prefix+z"),
        ("maximize", "prefix+m"),
        ("show_all", "prefix+n"),
        ("minimize", "prefix+shift+m"),
        ("restore", "prefix+shift+u"),
        ("focus_left", "prefix+left"),
        ("focus_right", "prefix+right"),
        ("focus_up", "prefix+up"),
        ("focus_down", "prefix+down"),
        ("next_pane", "prefix+o"),
        ("prev_pane", "prefix+i"),
        ("new_tab", "prefix+c"),
        ("next_tab", "prefix+shift+n"),
        ("prev_tab", "prefix+p"),
        ("rename_tab", "prefix+shift+t"),
        ("palette", "prefix+k"),
        ("search", "prefix+/"),
        ("scroll_mode", "prefix+["),
        ("help", "f1"),
        ("copy", "ctrl+shift+c"),
        ("paste", "ctrl+shift+v"),
        ("quit", "prefix+shift+q"),
        ("swap_pane", "prefix+w"),
        ("move_pane", "prefix+shift+o"),
        ("resize_left", "prefix+shift+left"),
        ("resize_right", "prefix+shift+right"),
        ("resize_up", "prefix+shift+up"),
        ("resize_down", "prefix+shift+down"),
        ("balance", "prefix+="),
        ("cycle_theme", "prefix+t"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[derive(Debug, Clone)]
pub struct CompiledKeybinds {
    pub by_action: BTreeMap<String, Binding>,
    pub prefix: KeyChord,
}

impl CompiledKeybinds {
    pub fn action_for(&self, prefix: bool, chord: KeyChord) -> Option<&str> {
        self.by_action
            .iter()
            .find(|(_, binding)| binding.prefix == prefix && binding.chord == chord)
            .map(|(action, _)| action.as_str())
    }

    pub fn binding(&self, action: &str) -> Option<Binding> {
        self.by_action.get(action).copied()
    }

    /// Human-readable summary used by the help overlay and docs.
    pub fn describe(&self, action: &str) -> String {
        self.by_action
            .get(action)
            .map(|b| {
                if b.prefix {
                    format!("{} then {}", self.prefix, b.chord)
                } else {
                    b.chord.to_string()
                }
            })
            .unwrap_or_else(|| "unbound".to_string())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeybindError {
    #[error("unknown action '{0}'")]
    UnknownAction(String),
    #[error("invalid binding for '{action}': {source}")]
    InvalidBinding {
        action: String,
        #[source]
        source: KeyParseError,
    },
    #[error("duplicate binding '{binding}' used by '{first}' and '{second}'")]
    Duplicate {
        binding: String,
        first: String,
        second: String,
    },
}

pub fn compile(
    prefix: KeyChord,
    overrides: &BTreeMap<String, String>,
) -> Result<CompiledKeybinds, KeybindError> {
    let mut raw = default_keybinds();
    for (action, binding) in overrides {
        if !ACTIONS.contains(&action.as_str()) {
            return Err(KeybindError::UnknownAction(action.clone()));
        }
        raw.insert(action.clone(), binding.clone());
    }
    let mut by_action = BTreeMap::new();
    let mut seen: BTreeMap<(bool, KeyChord), String> = BTreeMap::new();
    for (action, text) in raw {
        if text.trim().eq_ignore_ascii_case(UNBOUND) {
            continue;
        }
        let binding = parse_binding(&text).map_err(|source| KeybindError::InvalidBinding {
            action: action.clone(),
            source,
        })?;
        if let Some(first) = seen.insert((binding.prefix, binding.chord), action.clone()) {
            return Err(KeybindError::Duplicate {
                binding: binding.to_string(),
                first,
                second: action,
            });
        }
        by_action.insert(action, binding);
    }
    Ok(CompiledKeybinds { by_action, prefix })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chords() {
        assert_eq!(
            parse_binding("ctrl+shift+c").unwrap(),
            Binding {
                prefix: false,
                chord: KeyChord::new(
                    Modifiers {
                        control: true,
                        alt: false,
                        shift: true
                    },
                    KeyCode::Char('c')
                )
            }
        );
        let b = parse_binding("prefix+Shift+Left").unwrap();
        assert!(b.prefix);
        assert_eq!(b.chord.code, KeyCode::Left);
        assert!(b.chord.mods.shift);
        assert_eq!(parse_binding("F1").unwrap().chord.code, KeyCode::F(1));
        assert_eq!(parse_binding("?").unwrap().chord.code, KeyCode::Char('?'));
        assert_eq!(
            parse_binding("prefix++").unwrap().chord.code,
            KeyCode::Char('+')
        );
    }

    #[test]
    fn none_unbinds_an_action_and_frees_its_key() {
        let prefix = parse_prefix("ctrl+space").unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("help".to_string(), "none".to_string());
        // The freed F1 can now be reused by another action.
        overrides.insert("palette".to_string(), "F1".to_string());
        let compiled = compile(prefix, &overrides).unwrap();
        assert!(compiled.binding("help").is_none());
        assert_eq!(compiled.describe("help"), "unbound");
        assert_eq!(
            compiled.action_for(false, KeyChord::new(Modifiers::NONE, KeyCode::F(1))),
            Some("palette")
        );
    }

    #[test]
    fn uppercase_implies_shift() {
        let b = parse_binding("prefix+X").unwrap();
        assert!(b.chord.mods.shift);
        assert_eq!(b.chord.code, KeyCode::Char('x'));
    }

    #[test]
    fn bad_chords_are_rejected() {
        assert_eq!(parse_binding(""), Err(KeyParseError::Empty));
        assert!(parse_binding("ctrl+").is_err());
        assert!(parse_binding("ctrl+alt+shift+a+b").is_err());
        assert!(parse_binding("f25").is_err());
        assert!(parse_binding("prefix+prefix+a").is_err());
    }

    #[test]
    fn defaults_are_valid_and_unique() {
        let prefix = parse_prefix("ctrl+space").unwrap();
        let compiled = compile(prefix, &BTreeMap::new()).unwrap();
        assert_eq!(compiled.by_action.len(), ACTIONS.len());
        assert_eq!(
            compiled.action_for(true, KeyChord::new(Modifiers::NONE, KeyCode::Char('|'))),
            Some("split_right")
        );
        assert_eq!(
            compiled.action_for(true, KeyChord::new(Modifiers::NONE, KeyCode::Char('['))),
            Some("scroll_mode")
        );
        assert_eq!(
            compiled.action_for(
                false,
                KeyChord::new(Modifiers::CONTROL | Modifiers::SHIFT, KeyCode::Char('c'))
            ),
            Some("copy")
        );
    }

    #[test]
    fn duplicate_overrides_fail() {
        let prefix = parse_prefix("ctrl+space").unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("split_right".to_string(), "f1".to_string());
        assert!(matches!(
            compile(prefix, &overrides),
            Err(KeybindError::Duplicate { .. })
        ));
    }
}
