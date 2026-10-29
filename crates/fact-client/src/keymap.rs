//! Crossterm event translation: key bindings and terminal input encoding.

use crossterm::event::{KeyCode as CKey, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use fact_config::keys::{KeyCode as FactKey, Modifiers};
use fact_config::KeyChord;
use fact_protocol::pane_modes;

/// Tracking mode and report encoding are negotiated independently by the app.
pub fn encode_mouse(
    modes: u32,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> Option<Vec<u8>> {
    if modes & pane_modes::SGR_MOUSE != 0 {
        return match kind {
            MouseEventKind::Down(button) => Some(sgr_mouse(button, column, row, true, modifiers)),
            MouseEventKind::Up(button) => Some(sgr_mouse(button, column, row, false, modifiers)),
            MouseEventKind::Drag(button) => Some(sgr_mouse_motion(button, column, row, modifiers)),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => Some(sgr_wheel(
                kind == MouseEventKind::ScrollUp,
                column,
                row,
                modifiers,
            )),
            MouseEventKind::Moved => Some(
                format!(
                    "\x1b[<{};{};{}M",
                    35 + mouse_modifiers(modifiers),
                    column.saturating_add(1),
                    row.saturating_add(1)
                )
                .into_bytes(),
            ),
            _ => None,
        };
    }
    // The original X11 encoding has a one-byte coordinate limit. Dropping an
    // out-of-range report is safer than wrapping it to a different cell.
    if column > 222 || row > 222 {
        return None;
    }
    let button_code = |button| match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let code = match kind {
        MouseEventKind::Down(button) => button_code(button),
        MouseEventKind::Up(_) => 3,
        MouseEventKind::Drag(button) => 32 + button_code(button),
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        _ => return None,
    } + mouse_modifiers(modifiers);
    Some(vec![
        0x1b,
        b'[',
        b'M',
        code + 32,
        (column + 33) as u8,
        (row + 33) as u8,
    ])
}

fn mouse_modifiers(modifiers: KeyModifiers) -> u8 {
    u8::from(modifiers.contains(KeyModifiers::SHIFT)) * 4
        + u8::from(modifiers.contains(KeyModifiers::ALT)) * 8
        + u8::from(modifiers.contains(KeyModifiers::CONTROL)) * 16
}

pub fn chord_for(key: KeyEvent) -> Option<KeyChord> {
    let code = match key.code {
        CKey::Char(' ') => FactKey::Space,
        CKey::Char(c) if c.is_ascii_uppercase() => FactKey::Char(c.to_ascii_lowercase()),
        CKey::Char(c) => FactKey::Char(c),
        CKey::F(n) => FactKey::F(n),
        CKey::Enter => FactKey::Enter,
        CKey::Tab | CKey::BackTab => FactKey::Tab,
        CKey::Esc => FactKey::Esc,
        CKey::Backspace => FactKey::Backspace,
        CKey::Delete => FactKey::Delete,
        CKey::Home => FactKey::Home,
        CKey::End => FactKey::End,
        CKey::PageUp => FactKey::PageUp,
        CKey::PageDown => FactKey::PageDown,
        CKey::Insert => FactKey::Insert,
        CKey::Left => FactKey::Left,
        CKey::Right => FactKey::Right,
        CKey::Up => FactKey::Up,
        CKey::Down => FactKey::Down,
        _ => return None,
    };

    let mut modifiers = Modifiers {
        control: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    };

    // Crossterm reports Shift for shifted symbols (`|`, `?`, `+`). Bindings
    // spell those as the literal key with no modifier, so normalize them the
    // same way the config parser does. Letters keep Shift because the parser
    // folds `Shift+X` into lowercase + shift.
    if matches!(code, FactKey::Char(c) if !c.is_ascii_alphabetic()) {
        modifiers.shift = false;
    }
    if matches!(key.code, CKey::BackTab) {
        modifiers.shift = true;
    }
    Some(KeyChord::new(modifiers, code))
}

/// Windows reports AltGr as Ctrl+Alt. A Ctrl+Alt chord that produced a
/// non-letter character (`@`, `{`, `ą`, `€`) is typed text, not a shortcut.
fn is_altgr_text(key: &KeyEvent) -> bool {
    matches!(key.code, CKey::Char(c) if !c.is_ascii_alphabetic() && !c.is_control())
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT)
}

fn control_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => (c as u8) & 0x1f,
        'A'..='Z' => (c.to_ascii_lowercase() as u8) & 0x1f,
        '@' | ' ' => 0,
        '[' => 27,
        '\\' => 28,
        ']' => 29,
        '^' => 30,
        '_' => 31,
        '?' => 127,
        _ => return None,
    })
}

pub fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if is_altgr_text(&key) {
        let CKey::Char(c) = key.code else {
            return None;
        };
        let mut buffer = [0u8; 4];
        return Some(c.encode_utf8(&mut buffer).as_bytes().to_vec());
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        CKey::Char(c) if control => out.push(control_byte(c)?),
        CKey::Char(c) => {
            let mut buffer = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
        }
        CKey::Enter => out.push(b'\r'),
        CKey::Tab => out.push(b'\t'),
        CKey::BackTab => out.extend_from_slice(b"\x1b[Z"),
        CKey::Esc => out.push(0x1b),
        CKey::Backspace => out.push(0x7f),
        CKey::Up => cursor_key(&mut out, b'A', app_cursor, key.modifiers),
        CKey::Down => cursor_key(&mut out, b'B', app_cursor, key.modifiers),
        CKey::Right => cursor_key(&mut out, b'C', app_cursor, key.modifiers),
        CKey::Left => cursor_key(&mut out, b'D', app_cursor, key.modifiers),
        CKey::Home => cursor_key(&mut out, b'H', app_cursor, key.modifiers),
        CKey::End => cursor_key(&mut out, b'F', app_cursor, key.modifiers),
        CKey::Insert => tilde_key(&mut out, 2, key.modifiers),
        CKey::Delete => tilde_key(&mut out, 3, key.modifiers),
        CKey::PageUp => tilde_key(&mut out, 5, key.modifiers),
        CKey::PageDown => tilde_key(&mut out, 6, key.modifiers),
        CKey::F(n) => function_key(&mut out, n, key.modifiers),
        _ => return None,
    }
    Some(out)
}

/// Encode the keys that legacy VT bytes cannot express for an app that turned
/// on the kitty keyboard protocol: Enter, Tab and Backspace with modifiers
/// become `CSI code ; mods u`, so Shift+Enter inserts a newline in Claude
/// Code, OpenCode or Copilot instead of submitting. Unmodified keys, Shift+Tab
/// (`CSI Z`) and everything else keep their usual encoding, as the protocol's
/// "disambiguate" level allows.
pub fn encode_key_kitty(key: KeyEvent) -> Option<Vec<u8>> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let code = match key.code {
        CKey::Enter if shift || alt || control => 13,
        CKey::Backspace if shift || alt || control => 127,
        CKey::Tab if alt || control => 9,
        CKey::BackTab if alt || control => 9,
        _ => return None,
    };
    let shift = shift || key.code == CKey::BackTab;
    let mods = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    Some(format!("\x1b[{code};{mods}u").into_bytes())
}

// Windows virtual-key codes and control-key-state flags used by
// win32-input-mode (`CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`).
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0d;
const VK_ESCAPE: u16 = 0x1b;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2d;
const VK_DELETE: u16 = 0x2e;
const VK_F1: u16 = 0x70;
const LEFT_ALT_PRESSED: u32 = 0x0002;
const LEFT_CTRL_PRESSED: u32 = 0x0008;
const SHIFT_PRESSED: u32 = 0x0010;
const ENHANCED_KEY: u32 = 0x0100;

/// Encode a key press for a pane whose ConPTY enabled win32-input-mode.
///
/// Windows Terminal sends every key this way, which is why console programs
/// there can tell Shift+Enter, Ctrl+Enter or Ctrl+Backspace apart. FACT keeps
/// ordinary text on the VT path (ConPTY turns it into identical key records)
/// and sends a down/up record pair for everything VT bytes would flatten.
pub fn encode_key_win32(key: KeyEvent) -> Option<Vec<u8>> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT) || key.code == CKey::BackTab;
    let (vk, uc, enhanced) = match key.code {
        CKey::Char(_) if is_altgr_text(&key) || (!control && !alt) => {
            return encode_key(key, false);
        }
        CKey::Char(c) => {
            let uc = if control {
                control_byte(c).map_or(0, u16::from)
            } else {
                u16::try_from(u32::from(c)).unwrap_or(0)
            };
            let vk = if c == ' ' { VK_SPACE } else { vk_for_char(c) };
            (vk, uc, false)
        }
        CKey::Enter => (VK_RETURN, if control { 0x0a } else { 0x0d }, false),
        CKey::Tab | CKey::BackTab => (VK_TAB, if control { 0 } else { 0x09 }, false),
        CKey::Backspace => (VK_BACK, if control { 0x7f } else { 0x08 }, false),
        CKey::Esc => (VK_ESCAPE, 0x1b, false),
        CKey::Left => (VK_LEFT, 0, true),
        CKey::Up => (VK_UP, 0, true),
        CKey::Right => (VK_RIGHT, 0, true),
        CKey::Down => (VK_DOWN, 0, true),
        CKey::Home => (VK_HOME, 0, true),
        CKey::End => (VK_END, 0, true),
        CKey::PageUp => (VK_PRIOR, 0, true),
        CKey::PageDown => (VK_NEXT, 0, true),
        CKey::Insert => (VK_INSERT, 0, true),
        CKey::Delete => (VK_DELETE, 0, true),
        CKey::F(n @ 1..=24) => (VK_F1 + u16::from(n) - 1, 0, false),
        _ => return None,
    };
    let state = u32::from(alt) * LEFT_ALT_PRESSED
        + u32::from(control) * LEFT_CTRL_PRESSED
        + u32::from(shift) * SHIFT_PRESSED
        + u32::from(enhanced) * ENHANCED_KEY;
    let scan = scan_code(vk);
    Some(
        format!("\x1b[{vk};{scan};{uc};1;{state};1_\x1b[{vk};{scan};{uc};0;{state};1_")
            .into_bytes(),
    )
}

#[cfg(windows)]
fn vk_for_char(c: char) -> u16 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
    let Ok(unit) = u16::try_from(u32::from(c)) else {
        return 0;
    };
    // Low byte is the virtual key on the active layout; -1 means unmapped.
    let scanned = unsafe { VkKeyScanW(unit) };
    if scanned == -1 {
        0
    } else {
        (scanned as u16) & 0xff
    }
}

#[cfg(not(windows))]
fn vk_for_char(c: char) -> u16 {
    if c.is_ascii_alphanumeric() {
        c.to_ascii_uppercase() as u16
    } else {
        0
    }
}

#[cfg(windows)]
fn scan_code(vk: u16) -> u32 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
    if vk == 0 {
        return 0;
    }
    unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) }
}

#[cfg(not(windows))]
fn scan_code(_vk: u16) -> u32 {
    0
}

fn modifier_code(modifiers: KeyModifiers) -> u8 {
    1 + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
}

fn cursor_key(out: &mut Vec<u8>, letter: u8, app_cursor: bool, modifiers: KeyModifiers) {
    let relevant = modifiers & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
    if relevant.is_empty() {
        if app_cursor {
            out.extend_from_slice(b"\x1bO");
        } else {
            out.extend_from_slice(b"\x1b[");
        }
        out.push(letter);
    } else {
        out.extend_from_slice(
            format!("\x1b[1;{}{}", modifier_code(modifiers), letter as char).as_bytes(),
        );
    }
}

fn tilde_key(out: &mut Vec<u8>, number: u8, modifiers: KeyModifiers) {
    let relevant = modifiers & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
    if relevant.is_empty() {
        out.extend_from_slice(format!("\x1b[{number}~").as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{number};{}~", modifier_code(modifiers)).as_bytes());
    }
}

fn function_key(out: &mut Vec<u8>, number: u8, modifiers: KeyModifiers) {
    let relevant = modifiers & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
    let modifier = modifier_code(modifiers);
    let code = match number {
        1..=4 => {
            let letter = b'P' + (number - 1);
            if relevant.is_empty() {
                out.extend_from_slice(b"\x1bO");
                out.push(letter);
            } else {
                out.extend_from_slice(format!("\x1b[1;{}{}", modifier, letter as char).as_bytes());
            }
            return;
        }
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        13 => 25,
        14 => 26,
        15 => 28,
        16 => 29,
        17 => 31,
        18 => 32,
        19 => 33,
        20 => 34,
        _ => return,
    };
    if relevant.is_empty() {
        out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{code};{modifier}~").as_bytes());
    }
}

pub fn sgr_mouse(
    button: MouseButton,
    column: u16,
    row: u16,
    pressed: bool,
    modifiers: KeyModifiers,
) -> Vec<u8> {
    let base = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let modifier_code = mouse_modifiers(modifiers);
    let terminator = if pressed { 'M' } else { 'm' };
    format!(
        "\x1b[<{};{};{}{terminator}",
        base + modifier_code,
        column.saturating_add(1),
        row.saturating_add(1)
    )
    .into_bytes()
}

/// SGR mouse motion with the left button held. The xterm protocol sets bit 32
/// on motion events; sending a fresh "press" for every drag step makes editors
/// treat each step as a new click.
pub fn sgr_mouse_motion(
    button: MouseButton,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> Vec<u8> {
    let base = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let modifier_code = mouse_modifiers(modifiers);
    format!(
        "\x1b[<{};{};{}M",
        base + 32 + modifier_code,
        column.saturating_add(1),
        row.saturating_add(1)
    )
    .into_bytes()
}

pub fn sgr_wheel(up: bool, column: u16, row: u16, modifiers: KeyModifiers) -> Vec<u8> {
    let base = if up { 64 } else { 65 };
    let modifier_code = mouse_modifiers(modifiers);
    format!(
        "\x1b[<{};{};{}M",
        base + modifier_code,
        column.saturating_add(1),
        row.saturating_add(1)
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: CKey, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn chords_normalize_shifted_symbols() {
        let chord = chord_for(key(CKey::Char('|'), KeyModifiers::SHIFT)).unwrap();
        assert_eq!(chord.code, FactKey::Char('|'));
        assert!(!chord.mods.shift);

        let chord = chord_for(key(CKey::Char('X'), KeyModifiers::SHIFT)).unwrap();
        assert_eq!(chord.code, FactKey::Char('x'));
        assert!(chord.mods.shift);
    }

    #[test]
    fn encodes_common_terminal_keys() {
        assert_eq!(
            encode_key(key(CKey::Char('c'), KeyModifiers::CONTROL), false).unwrap(),
            b"\x03"
        );
        assert_eq!(
            encode_key(key(CKey::Char('x'), KeyModifiers::ALT), false).unwrap(),
            b"\x1bx"
        );
        assert_eq!(
            encode_key(key(CKey::Up, KeyModifiers::NONE), false).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_key(key(CKey::Up, KeyModifiers::NONE), true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(key(CKey::Up, KeyModifiers::CONTROL), false).unwrap(),
            b"\x1b[1;5A"
        );
        assert_eq!(
            encode_key(key(CKey::BackTab, KeyModifiers::SHIFT), false).unwrap(),
            b"\x1b[Z"
        );
        assert!(encode_key(key(CKey::F(1), KeyModifiers::NONE), false)
            .unwrap()
            .starts_with(b"\x1bOP"));
    }

    fn records(bytes: &[u8]) -> Vec<Vec<u32>> {
        let text = std::str::from_utf8(bytes).unwrap();
        text.split('_')
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.trim_start_matches("\x1b[")
                    .split(';')
                    .map(|n| n.parse().unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn win32_input_keeps_modifiers_that_vt_drops() {
        let shift_enter = encode_key_win32(key(CKey::Enter, KeyModifiers::SHIFT)).unwrap();
        let parts = records(&shift_enter);
        assert_eq!(parts.len(), 2, "a down and an up record");
        assert_eq!(parts[0][0], 0x0d);
        assert_eq!(parts[0][2], 13);
        assert_eq!(parts[0][3], 1);
        assert_eq!(parts[0][4] & SHIFT_PRESSED, SHIFT_PRESSED);
        assert_eq!(parts[1][3], 0);

        let ctrl_back = encode_key_win32(key(CKey::Backspace, KeyModifiers::CONTROL)).unwrap();
        let parts = records(&ctrl_back);
        assert_eq!((parts[0][0], parts[0][2]), (0x08, 0x7f));
        assert_eq!(parts[0][4] & LEFT_CTRL_PRESSED, LEFT_CTRL_PRESSED);

        let ctrl_enter = encode_key_win32(key(CKey::Enter, KeyModifiers::CONTROL)).unwrap();
        assert_eq!(records(&ctrl_enter)[0][2], 0x0a);

        let up = encode_key_win32(key(CKey::Up, KeyModifiers::NONE)).unwrap();
        assert_eq!(records(&up)[0][4], ENHANCED_KEY);

        let ctrl_a = encode_key_win32(key(CKey::Char('a'), KeyModifiers::CONTROL)).unwrap();
        let parts = records(&ctrl_a);
        assert_eq!((parts[0][0], parts[0][2]), (0x41, 1));

        let back_tab = encode_key_win32(key(CKey::BackTab, KeyModifiers::SHIFT)).unwrap();
        assert_eq!(records(&back_tab)[0][4] & SHIFT_PRESSED, SHIFT_PRESSED);
    }

    #[test]
    fn kitty_encodes_only_the_ambiguous_modified_keys() {
        let encode = |code, modifiers| encode_key_kitty(key(code, modifiers));
        assert_eq!(
            encode(CKey::Enter, KeyModifiers::SHIFT).unwrap(),
            b"\x1b[13;2u"
        );
        assert_eq!(
            encode(CKey::Enter, KeyModifiers::CONTROL).unwrap(),
            b"\x1b[13;5u"
        );
        assert_eq!(
            encode(CKey::Enter, KeyModifiers::ALT).unwrap(),
            b"\x1b[13;3u"
        );
        assert_eq!(
            encode(CKey::Backspace, KeyModifiers::CONTROL).unwrap(),
            b"\x1b[127;5u"
        );
        assert_eq!(
            encode(CKey::Tab, KeyModifiers::CONTROL).unwrap(),
            b"\x1b[9;5u"
        );
        assert_eq!(
            encode(CKey::BackTab, KeyModifiers::CONTROL | KeyModifiers::SHIFT).unwrap(),
            b"\x1b[9;6u"
        );
        // Plain keys stay legacy so a crashed app cannot break the shell.
        assert!(encode(CKey::Enter, KeyModifiers::NONE).is_none());
        assert!(encode(CKey::Backspace, KeyModifiers::NONE).is_none());
        assert!(encode(CKey::BackTab, KeyModifiers::SHIFT).is_none());
        assert!(encode(CKey::Char('c'), KeyModifiers::CONTROL).is_none());
    }

    #[test]
    fn plain_and_altgr_text_stay_text() {
        assert_eq!(
            encode_key_win32(key(CKey::Char('x'), KeyModifiers::NONE)).unwrap(),
            b"x"
        );
        assert_eq!(
            encode_key_win32(key(CKey::Char('X'), KeyModifiers::SHIFT)).unwrap(),
            b"X"
        );
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(
            encode_key(key(CKey::Char('@'), altgr), false).unwrap(),
            b"@"
        );
        assert_eq!(
            encode_key(key(CKey::Char('ą'), altgr), false).unwrap(),
            "ą".as_bytes()
        );
        assert_eq!(
            encode_key_win32(key(CKey::Char('€'), altgr)).unwrap(),
            "€".as_bytes()
        );
        // A real Ctrl+Alt+letter chord is still a control sequence.
        assert_eq!(
            encode_key(key(CKey::Char('a'), altgr), false).unwrap(),
            b"\x1b\x01"
        );
    }

    #[test]
    fn mouse_encoding_is_one_based() {
        let sgr = sgr_mouse(MouseButton::Left, 4, 2, true, KeyModifiers::NONE);
        assert_eq!(sgr, b"\x1b[<0;5;3M");
        let sgr = sgr_mouse(MouseButton::Right, 0, 0, false, KeyModifiers::CONTROL);
        assert_eq!(sgr, b"\x1b[<18;1;1m");
        let sgr = sgr_mouse_motion(MouseButton::Left, 4, 2, KeyModifiers::NONE);
        assert_eq!(sgr, b"\x1b[<32;5;3M");
    }

    #[test]
    fn mouse_reports_honor_the_negotiated_encoding() {
        let events = [
            (MouseEventKind::Down(MouseButton::Left), 0, "\x1b[<0;5;3M"),
            (MouseEventKind::Up(MouseButton::Left), 3, "\x1b[<0;5;3m"),
            (MouseEventKind::Drag(MouseButton::Left), 32, "\x1b[<32;5;3M"),
            (MouseEventKind::Moved, 35, "\x1b[<35;5;3M"),
            (MouseEventKind::ScrollUp, 64, "\x1b[<64;5;3M"),
            (MouseEventKind::ScrollDown, 65, "\x1b[<65;5;3M"),
        ];
        for (kind, code, sgr) in events {
            assert_eq!(
                encode_mouse(pane_modes::SGR_MOUSE, kind, 4, 2, KeyModifiers::NONE).unwrap(),
                sgr.as_bytes()
            );
            assert_eq!(
                encode_mouse(0, kind, 4, 2, KeyModifiers::NONE).unwrap(),
                vec![0x1b, b'[', b'M', code + 32, 37, 35]
            );
        }
        assert!(encode_mouse(0, MouseEventKind::ScrollUp, 223, 0, KeyModifiers::NONE).is_none());
        assert!(encode_mouse(
            pane_modes::SGR_MOUSE,
            MouseEventKind::ScrollUp,
            400,
            0,
            KeyModifiers::NONE
        )
        .is_some());
        assert_eq!(
            encode_mouse(0, MouseEventKind::ScrollUp, 0, 0, KeyModifiers::CONTROL).unwrap(),
            b"\x1b[Mp!!"
        );
    }
}
