//! Key names and shortcut parsing, ported from pynput's `Key`/`HotKey.parse`.
//!
//! Single keys keep their side (`alt_r` is not `alt_l`) because hold and
//! command keys match the physical key. Chords are compared on canonical
//! keys (side-less modifiers, lower-case characters) as a *set*, so
//! `<alt>+<ctrl>+2` and `Control+Option+2` are the same chord.

use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Cmd,
}

impl Modifier {
    pub fn name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Alt => "alt",
            Modifier::Shift => "shift",
            Modifier::Cmd => "cmd",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    /// pynput's bare `alt`/`ctrl`/`shift`/`cmd` (the left key on macOS).
    Left,
    Right,
    /// Canonical, side-less form used inside chords.
    Any,
}

/// pynput `Key` members other than modifiers.
pub const NAMED_KEYS: &[&str] = &[
    "alt_gr",
    "backspace",
    "caps_lock",
    "delete",
    "down",
    "end",
    "enter",
    "esc",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "f13",
    "f14",
    "f15",
    "f16",
    "f17",
    "f18",
    "f19",
    "f20",
    "home",
    "insert",
    "left",
    "menu",
    "num_lock",
    "page_down",
    "page_up",
    "pause",
    "print_screen",
    "right",
    "scroll_lock",
    "space",
    "tab",
    "up",
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    Mod(Modifier, Side),
    Named(&'static str),
    Char(char),
}

impl Key {
    pub const ESC: Key = Key::Named("esc");

    /// `getattr(keyboard.Key, name)` or a single character (`_key_from_name`).
    pub fn from_name(name: &str) -> Option<Key> {
        // macOS: `Key.alt_gr` is the same vk (0x3D) as `Key.alt_r`.
        if cfg!(target_os = "macos") && name == "alt_gr" {
            return Some(Key::Mod(Modifier::Alt, Side::Right));
        }
        let (base, side) = match name {
            n if n.ends_with("_l") => (&n[..n.len() - 2], Side::Left),
            n if n.ends_with("_r") => (&n[..n.len() - 2], Side::Right),
            n => (n, Side::Left),
        };
        let modifier = match base {
            "ctrl" => Some(Modifier::Ctrl),
            "alt" => Some(Modifier::Alt),
            "shift" => Some(Modifier::Shift),
            "cmd" => Some(Modifier::Cmd),
            _ => None,
        };
        if let Some(modifier) = modifier {
            return Some(Key::Mod(modifier, side));
        }
        if let Some(found) = NAMED_KEYS.iter().find(|k| **k == name) {
            return Some(Key::Named(found));
        }
        let mut chars = name.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Key::Char(c)),
            _ => None,
        }
    }

    /// pynput `Listener.canonical`: side-less modifiers, lower-case chars.
    pub fn canonical(&self) -> Key {
        match self {
            Key::Mod(m, _) => Key::Mod(*m, Side::Any),
            Key::Char(c) => Key::Char(c.to_lowercase().next().unwrap_or(*c)),
            other => other.clone(),
        }
    }
}

/// The keys one key event presses: `base` (the key at its US-layout
/// position), plus the character Shift typed there when it is a different
/// printable one. pynput reports that character ("+" for Shift+"="), so
/// `<ctrl>++` matches as in Python while `<ctrl>+<shift>+=` keeps matching.
/// Option is left out: it would turn Option+2 into "™".
pub fn keys_for_typed(base: Key, shift: bool, option: bool, typed: &str) -> Vec<Key> {
    let mut chars = typed.chars();
    let extra = match (&base, chars.next(), chars.next()) {
        (Key::Char(_), Some(c), None)
            if shift && !option && !c.is_control() && !c.is_whitespace() =>
        {
            Some(Key::Char(c)).filter(|k| k.canonical() != base.canonical())
        }
        _ => None,
    };
    let mut keys = vec![base];
    keys.extend(extra);
    keys
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Mod(m, Side::Right) => write!(f, "<{}_r>", m.name()),
            Key::Mod(m, _) => write!(f, "<{}>", m.name()),
            Key::Named(n) => write!(f, "<{n}>"),
            Key::Char(c) => write!(f, "{c}"),
        }
    }
}

/// `is_hold_key`: a bare key name (`alt_r`, `f19`) means hold mode.
pub fn is_hold_key(hotkey: &str) -> bool {
    !hotkey.is_empty()
        && !hotkey.contains('+')
        && hotkey.chars().count() > 1
        && Key::from_name(hotkey).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutError(pub String);

impl fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ShortcutError {}

fn human_alias(token: &str) -> Option<Key> {
    let modifier = match token {
        "ctrl" | "control" | "ctl" | "⌃" => Modifier::Ctrl,
        "alt" | "option" | "opt" | "⌥" => Modifier::Alt,
        "shift" | "⇧" => Modifier::Shift,
        "cmd" | "command" | "super" | "win" | "meta" | "⌘" => Modifier::Cmd,
        "escape" => return Some(Key::ESC),
        "return" => return Some(Key::Named("enter")),
        "spacebar" => return Some(Key::Named("space")),
        _ => return None,
    };
    Some(Key::Mod(modifier, Side::Left))
}

/// A parsed chord: the set of canonical keys that must all be down.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    keys: BTreeSet<Key>,
}

impl Chord {
    /// `HotKey.parse` plus the human spellings the settings UI accepts
    /// (`Control+Option+2`, `ctrl+alt+d`). Modifier order is irrelevant.
    pub fn parse(shortcut: &str) -> Result<Chord, ShortcutError> {
        let invalid = || ShortcutError(format!("invalid shortcut {shortcut:?}"));
        let trimmed = shortcut.trim();
        if trimmed.is_empty() {
            return Err(invalid());
        }
        let mut keys = BTreeSet::new();
        // A trailing "+" is the plus key itself ("<ctrl>++").
        let mut tokens: Vec<String> = trimmed.split('+').map(str::to_string).collect();
        if trimmed.ends_with("++") {
            tokens.truncate(tokens.len() - 2);
            tokens.push("+".to_string());
        }
        for raw in tokens {
            let token = raw.trim();
            if token.is_empty() {
                return Err(invalid());
            }
            let key = if let Some(inner) = token.strip_prefix('<').and_then(|t| t.strip_suffix('>'))
            {
                Key::from_name(&inner.to_lowercase())
                    .filter(|k| !matches!(k, Key::Char(_)))
                    .ok_or_else(invalid)?
            } else if token.chars().count() == 1 {
                Key::Char(token.chars().next().unwrap_or_default())
            } else {
                let lower = token.to_lowercase();
                human_alias(&lower)
                    .or_else(|| Key::from_name(&lower).filter(|k| !matches!(k, Key::Char(_))))
                    .ok_or_else(invalid)?
            };
            if !keys.insert(key.canonical()) {
                return Err(ShortcutError(format!(
                    "repeated key in shortcut {shortcut:?}"
                )));
            }
        }
        Ok(Chord { keys })
    }

    pub fn keys(&self) -> &BTreeSet<Key> {
        &self.keys
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.keys.contains(key)
    }

    /// Stable pynput spelling: modifiers ctrl, alt, shift, cmd, then the rest.
    pub fn canonical(&self) -> String {
        self.keys
            .iter()
            .map(Key::to_string)
            .collect::<Vec<_>>()
            .join("+")
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Canonical spelling of any shortcut string, or None when it doesn't parse.
pub fn canonical_shortcut(shortcut: &str) -> Option<String> {
    if is_hold_key(shortcut) {
        return Key::from_name(shortcut).map(|k| k.to_string());
    }
    Chord::parse(shortcut).ok().map(|c| c.canonical())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_order_is_irrelevant() {
        let a = Chord::parse("Option+Control+2").unwrap();
        let b = Chord::parse("Control+Option+2").unwrap();
        let c = Chord::parse("<ctrl>+<alt>+2").unwrap();
        let d = Chord::parse("<alt>+<ctrl>+2").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
        assert_eq!(a.canonical(), "<ctrl>+<alt>+2");
    }

    #[test]
    fn sides_collapse_inside_chords_only() {
        assert_eq!(
            Chord::parse("<ctrl_r>+d").unwrap(),
            Chord::parse("<ctrl>+d").unwrap()
        );
        assert_ne!(Key::from_name("alt_r"), Key::from_name("alt_l"));
        assert_eq!(Key::from_name("alt"), Key::from_name("alt_l"));
    }

    #[test]
    fn rejects_bad_shortcuts() {
        for bad in [
            "",
            "+",
            "<ctrl>+",
            "<nope>+a",
            "<ctrl>+<ctrl>+a",
            "ctrl+bogus",
        ] {
            assert!(Chord::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Chord::parse("<ctrl>++").unwrap().canonical(), "<ctrl>++");
    }

    #[test]
    fn hold_keys() {
        assert!(is_hold_key("alt_r"));
        assert!(is_hold_key("f19"));
        assert!(!is_hold_key("<ctrl>+<alt>+d"));
        assert!(!is_hold_key(""));
        assert!(!is_hold_key("bogus"));
        assert_eq!(canonical_shortcut("alt_r").as_deref(), Some("<alt_r>"));
        assert_eq!(
            canonical_shortcut("Shift+Cmd+P").as_deref(),
            Some("<shift>+<cmd>+p")
        );
    }
}
