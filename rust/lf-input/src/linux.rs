//! Linux global shortcuts for the Rust engine.
//!
//! X11 only: a listen-only XInput2 *raw* key event subscription on the root
//! window through `x11rb`'s pure-Rust connection. Nothing is grabbed, no
//! subprocess is spawned and no extra permission (root, `input` group,
//! `/dev/input`) is needed. Wayland compositors do not deliver other apps'
//! keys to clients (XWayland only sees keys aimed at X11 windows), so a
//! Wayland session is reported as [`Session::Wayland`] and the listener
//! refuses to start with an explicit [`InputError::Unsupported`] instead of
//! claiming global shortcuts work.
//!
//! The event source only turns key codes into [`Key`]s for the
//! [`KeyboardHub`]; key events are never logged and no text is collected.
//! The translation and the cancellable pump are platform-independent so they
//! are tested with fakes (no display, no real keyboard).

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::hub::KeyboardHub;
use crate::keys::{keys_for_typed, Key, Modifier, Side};
use crate::listener::InputError;

/// The desktop session kind, as far as global key access is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    X11,
    Wayland,
    /// No `DISPLAY` / `WAYLAND_DISPLAY` (a console, SSH, a service).
    Unknown,
}

impl Session {
    pub fn as_str(self) -> &'static str {
        match self {
            Session::X11 => "x11",
            Session::Wayland => "wayland",
            Session::Unknown => "unknown",
        }
    }
}

pub const WAYLAND_UNSUPPORTED: &str = "Wayland doesn't let apps see keys pressed in other apps, \
so hold-to-talk and global shortcuts can't work in this session. Log in with an X11 session \
to use them.";
pub const NO_DISPLAY: &str =
    "No X11 display was found (DISPLAY is not set), so global shortcuts can't be listened for.";

/// `WAYLAND_DISPLAY` wins even when XWayland also sets `DISPLAY`: XWayland
/// only reports keys typed into X11 windows, never global ones.
pub fn detect_session(var: &dyn Fn(&str) -> Option<String>) -> Session {
    let set = |name: &str| var(name).is_some_and(|v| !v.trim().is_empty());
    if set("WAYLAND_DISPLAY") || var("XDG_SESSION_TYPE").as_deref() == Some("wayland") {
        Session::Wayland
    } else if set("DISPLAY") {
        Session::X11
    } else {
        Session::Unknown
    }
}

/// [`detect_session`] for this process's environment.
pub fn session() -> Session {
    detect_session(&|name| std::env::var(name).ok())
}

// ----------------------------------------------------------- key mapping ---

const XK_F1: u32 = 0xffbe;

/// An X11 keysym as a pynput-compatible [`Key`] (left/right modifiers keep
/// their side; Super/Meta are `cmd` like pynput's xorg backend).
pub fn key_for_keysym(sym: u32) -> Option<Key> {
    let m = |m, s| Some(Key::Mod(m, s));
    let named = |n| Key::from_name(n);
    match sym {
        0xffe1 => m(Modifier::Shift, Side::Left),
        0xffe2 => m(Modifier::Shift, Side::Right),
        0xffe3 => m(Modifier::Ctrl, Side::Left),
        0xffe4 => m(Modifier::Ctrl, Side::Right),
        0xffe9 => m(Modifier::Alt, Side::Left),
        0xffea => m(Modifier::Alt, Side::Right),
        0xffe7 | 0xffeb => m(Modifier::Cmd, Side::Left),
        0xffe8 | 0xffec => m(Modifier::Cmd, Side::Right),
        // ISO_Level3_Shift / Mode_switch: what the right Alt key types on
        // many non-US layouts (pynput's `alt_gr`).
        0xfe03 | 0xff7e => named("alt_gr"),
        0xffe5 => named("caps_lock"),
        0xff08 => named("backspace"),
        0xff09 => named("tab"),
        0xff0d | 0xff8d => named("enter"),
        0xff13 => named("pause"),
        0xff14 => named("scroll_lock"),
        0xff1b => named("esc"),
        0xffff => named("delete"),
        0xff50 => named("home"),
        0xff51 => named("left"),
        0xff52 => named("up"),
        0xff53 => named("right"),
        0xff54 => named("down"),
        0xff55 => named("page_up"),
        0xff56 => named("page_down"),
        0xff57 => named("end"),
        0xff61 => named("print_screen"),
        0xff63 => named("insert"),
        0xff67 => named("menu"),
        0xff7f => named("num_lock"),
        0x20 => named("space"),
        s if (XK_F1..XK_F1 + 20).contains(&s) => named(&format!("f{}", s - XK_F1 + 1)),
        s => char_for_keysym(s).map(Key::Char),
    }
}

/// The printable character of a keysym (Latin-1 or Unicode keysyms).
pub fn char_for_keysym(sym: u32) -> Option<char> {
    let code = match sym {
        0x21..=0x7e | 0xa0..=0xff => sym,
        s if (0x0100_0000..=0x0110_ffff).contains(&s) => s - 0x0100_0000,
        _ => return None,
    };
    char::from_u32(code).filter(|c| !c.is_control() && !c.is_whitespace())
}

fn is_shift(sym: u32) -> bool {
    matches!(sym, 0xffe1 | 0xffe2)
}

/// Turns physical key codes into hub keys: suppresses auto-repeat (a code
/// already down), remembers what each press pressed so its release releases
/// exactly that (even after Shift came up first), and can release everything
/// still held when the source stops, so a held gesture never sticks.
#[derive(Default)]
pub struct Translator {
    down: HashMap<u32, Vec<Key>>,
    shift: BTreeSet<u32>,
}

impl Translator {
    pub fn press(&mut self, code: u32, keysyms: [u32; 2]) -> Vec<Key> {
        if self.down.contains_key(&code) {
            return Vec::new(); // auto-repeat
        }
        let [base, shifted] = keysyms;
        let Some(key) = key_for_keysym(base) else {
            self.down.insert(code, Vec::new());
            return Vec::new();
        };
        if is_shift(base) {
            self.shift.insert(code);
        }
        let typed = if self.shift.is_empty() || is_shift(base) {
            String::new()
        } else {
            char_for_keysym(shifted)
                .map(String::from)
                .unwrap_or_default()
        };
        let keys = keys_for_typed(key, !self.shift.is_empty(), false, &typed);
        self.down.insert(code, keys.clone());
        keys
    }

    pub fn release(&mut self, code: u32, keysyms: [u32; 2]) -> Vec<Key> {
        self.shift.remove(&code);
        match self.down.remove(&code) {
            Some(keys) => keys.into_iter().rev().collect(),
            // A release whose press predates the listener: release the key so
            // a bare modifier key-up still ends a hold.
            None => key_for_keysym(keysyms[0]).into_iter().collect(),
        }
    }

    /// Everything still held, in reverse press order per key.
    pub fn release_all(&mut self) -> Vec<Key> {
        self.shift.clear();
        let mut codes: Vec<u32> = self.down.keys().copied().collect();
        codes.sort_unstable();
        codes
            .into_iter()
            .flat_map(|code| {
                self.down
                    .remove(&code)
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
            })
            .collect()
    }
}

// ------------------------------------------------------------ event pump ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawKey {
    Press(u32),
    Release(u32),
}

/// A source of physical key codes (the X server, or a fake in tests).
pub trait KeySource {
    /// The next key event, waiting at most `wait`; `Ok(None)` on timeout.
    fn next(&mut self, wait: Duration) -> Result<Option<RawKey>, String>;
    /// `[unshifted, shifted]` keysyms of a key code.
    fn keysyms(&self, code: u32) -> [u32; 2];
}

/// How long one wait for an event may block, so `stop` is noticed promptly.
pub const POLL: Duration = Duration::from_millis(50);

/// Feed `hub` from `source` until `stop` is set (`Ok`) or the source fails
/// (`Err`, after releasing every held key).
pub fn pump(
    source: &mut dyn KeySource,
    hub: &KeyboardHub,
    stop: &AtomicBool,
) -> Result<(), InputError> {
    let mut translator = Translator::default();
    let result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        match source.next(POLL) {
            Ok(None) => {}
            Ok(Some(RawKey::Press(code))) => {
                for key in translator.press(code, source.keysyms(code)) {
                    hub.press(key);
                }
            }
            Ok(Some(RawKey::Release(code))) => {
                for key in translator.release(code, source.keysyms(code)) {
                    hub.release(key);
                }
            }
            Err(error) => break Err(InputError::Backend(error)),
        }
    };
    for key in translator.release_all() {
        hub.release(key);
    }
    result
}

pub const XI_TOO_OLD: &str = "The X server's XInput is older than 2.1, so a key release \
during another app's keyboard grab could be missed. Global shortcuts aren't enabled.";

/// The version the server agreed to must be at least XI 2.1: before 2.1,
/// raw events aren't delivered while another client holds a grab (XI2
/// protocol, "Changes in version 2.1"), which could leave a hold stuck.
pub fn check_xi_version(major: u16, minor: u16) -> Result<(), InputError> {
    if (major, minor) >= (2, 1) {
        Ok(())
    } else {
        Err(InputError::Unsupported(XI_TOO_OLD))
    }
}

/// Refuse sessions where global keys can't be observed, with the reason.
pub fn check_session(session: Session) -> Result<(), InputError> {
    match session {
        Session::X11 => Ok(()),
        Session::Wayland => Err(InputError::Unsupported(WAYLAND_UNSUPPORTED)),
        Session::Unknown => Err(InputError::Unsupported(NO_DISPLAY)),
    }
}

/// Connect to X11, subscribe to raw key events, call `ready` once the server
/// accepted the subscription, then pump until `stop`.
#[cfg(target_os = "linux")]
pub fn run_x11(
    hub: &KeyboardHub,
    stop: &AtomicBool,
    ready: &(dyn Fn() + Send + Sync),
) -> Result<(), InputError> {
    check_session(session())?;
    let mut source = x11::X11Source::connect()?;
    ready();
    pump(&mut source, hub, stop)
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xinput::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    use super::{KeySource, RawKey};
    use crate::listener::InputError;

    pub struct X11Source {
        conn: RustConnection,
        min_keycode: u8,
        per_keycode: usize,
        keysyms: Vec<u32>,
    }

    fn backend(what: &str, error: impl std::fmt::Display) -> InputError {
        InputError::Backend(format!("{what}: {error}"))
    }

    impl X11Source {
        pub fn connect() -> Result<Self, InputError> {
            let (conn, screen) =
                x11rb::connect(None).map_err(|e| backend("couldn't connect to the X server", e))?;
            let root = conn
                .setup()
                .roots
                .get(screen)
                .map(|s| s.root)
                .ok_or_else(|| backend("X server", "no root window"))?;
            let version = conn
                // Advertise 2.2: from XI 2.1 on, root-window raw events are
                // delivered even while another client holds a grab, so a
                // release during a menu or drag isn't lost.
                .xinput_xi_query_version(2, 2)
                .map_err(|e| backend("XInput2 unavailable", e))?
                .reply()
                .map_err(|e| backend("XInput2 unavailable", e))?;
            super::check_xi_version(version.major_version, version.minor_version)?;
            let mask = xinput::EventMask {
                deviceid: u16::from(xinput::Device::ALL_MASTER),
                mask: vec![
                    xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE,
                ],
            };
            conn.xinput_xi_select_events(root, &[mask])
                .map_err(|e| backend("couldn't listen for keys", e))?
                .check()
                .map_err(|e| backend("couldn't listen for keys", e))?;
            let mut source = X11Source {
                min_keycode: conn.setup().min_keycode,
                per_keycode: 0,
                keysyms: Vec::new(),
                conn,
            };
            source.load_keymap()?;
            Ok(source)
        }

        fn load_keymap(&mut self) -> Result<(), InputError> {
            let setup = self.conn.setup();
            let (min, max) = (setup.min_keycode, setup.max_keycode);
            let reply = self
                .conn
                .get_keyboard_mapping(min, max.saturating_sub(min).saturating_add(1))
                .map_err(|e| backend("couldn't read the keyboard map", e))?
                .reply()
                .map_err(|e| backend("couldn't read the keyboard map", e))?;
            self.min_keycode = min;
            self.per_keycode = usize::from(reply.keysyms_per_keycode);
            self.keysyms = reply.keysyms;
            Ok(())
        }
    }

    impl KeySource for X11Source {
        fn next(&mut self, wait: Duration) -> Result<Option<RawKey>, String> {
            let deadline = Instant::now() + wait;
            loop {
                match self.conn.poll_for_event().map_err(|e| e.to_string())? {
                    Some(Event::XinputRawKeyPress(e)) => return Ok(Some(RawKey::Press(e.detail))),
                    Some(Event::XinputRawKeyRelease(e)) => {
                        return Ok(Some(RawKey::Release(e.detail)))
                    }
                    Some(Event::MappingNotify(_)) => {
                        self.load_keymap().map_err(|e| e.to_string())?;
                    }
                    Some(_) => {}
                    None => {
                        let now = Instant::now();
                        if now >= deadline {
                            return Ok(None);
                        }
                        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
                    }
                }
            }
        }

        fn keysyms(&self, code: u32) -> [u32; 2] {
            let Some(offset) = code
                .checked_sub(u32::from(self.min_keycode))
                .map(|o| o as usize * self.per_keycode)
            else {
                return [0, 0];
            };
            let at = |i: usize| {
                (i < self.per_keycode)
                    .then(|| self.keysyms.get(offset + i).copied())
                    .flatten()
                    .unwrap_or(0)
            };
            let shifted = match at(1) {
                0 => at(0),
                s => s,
            };
            [at(0), shifted]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Callback, HubActions, Shortcuts};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // Fake X keymap: code -> [unshifted, shifted].
    const SHIFT_L: u32 = 50;
    const SHIFT_R: u32 = 62;
    const CTRL_L: u32 = 37;
    const CTRL_R: u32 = 105;
    const ALT_L: u32 = 64;
    const ALT_R: u32 = 108;
    const SUPER_L: u32 = 133;
    const KEY_D: u32 = 40;
    const KEY_EQUAL: u32 = 21;
    const KEY_F19: u32 = 197;

    fn keymap(code: u32) -> [u32; 2] {
        match code {
            SHIFT_L => [0xffe1, 0xffe1],
            SHIFT_R => [0xffe2, 0xffe2],
            CTRL_L => [0xffe3, 0xffe3],
            CTRL_R => [0xffe4, 0xffe4],
            ALT_L => [0xffe9, 0xffe9],
            ALT_R => [0xffea, 0xffea],
            SUPER_L => [0xffeb, 0xffeb],
            KEY_D => [0x64, 0x44],
            KEY_EQUAL => [0x3d, 0x2b],
            KEY_F19 => [XK_F1 + 18, XK_F1 + 18],
            _ => [0, 0],
        }
    }

    struct FakeSource {
        events: VecDeque<Result<Option<RawKey>, String>>,
        stop_when_empty: Option<Arc<AtomicBool>>,
    }

    impl KeySource for FakeSource {
        fn next(&mut self, _: Duration) -> Result<Option<RawKey>, String> {
            match self.events.pop_front() {
                Some(event) => event,
                None => {
                    if let Some(stop) = &self.stop_when_empty {
                        stop.store(true, Ordering::Release);
                    }
                    Ok(None)
                }
            }
        }
        fn keysyms(&self, code: u32) -> [u32; 2] {
            keymap(code)
        }
    }

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<&'static str>>>);
    impl Log {
        fn cb(&self, name: &'static str) -> Callback {
            let log = self.0.clone();
            Arc::new(move || log.lock().unwrap().push(name))
        }
        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn hub(hold: &str, toggle: &str, combo: &str) -> (KeyboardHub, Log) {
        let log = Log::default();
        let mut actions = HubActions::noop();
        actions.on_down = log.cb("down");
        actions.on_up = log.cb("up");
        actions.on_toggle = log.cb("toggle");
        let shortcuts = Shortcuts {
            hold_key: hold.into(),
            toggle_combo: toggle.into(),
            combos: if combo.is_empty() {
                Vec::new()
            } else {
                vec![(combo.to_string(), log.cb("combo"))]
            },
            command_key: String::new(),
        };
        let (hub, warnings) = KeyboardHub::new(&shortcuts, actions);
        assert!(warnings.is_empty(), "{warnings:?}");
        (hub, log)
    }

    fn run(hub: &KeyboardHub, events: Vec<RawKey>) -> Result<(), InputError> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut source = FakeSource {
            events: events.into_iter().map(|e| Ok(Some(e))).collect(),
            stop_when_empty: Some(stop.clone()),
        };
        pump(&mut source, hub, &stop)
    }

    #[test]
    fn sessions_are_explicit_and_wayland_beats_xwayland() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(detect_session(&env(&[("DISPLAY", ":0")])), Session::X11);
        assert_eq!(
            detect_session(&env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")])),
            Session::Wayland
        );
        assert_eq!(
            detect_session(&env(&[("DISPLAY", ":1"), ("XDG_SESSION_TYPE", "wayland")])),
            Session::Wayland
        );
        assert_eq!(detect_session(&env(&[])), Session::Unknown);
        assert_eq!(detect_session(&env(&[("DISPLAY", " ")])), Session::Unknown);
        assert!(check_session(Session::X11).is_ok());
        assert_eq!(
            check_session(Session::Wayland),
            Err(InputError::Unsupported(WAYLAND_UNSUPPORTED))
        );
        assert!(check_session(Session::Unknown).is_err());
    }

    #[test]
    fn only_xi_2_1_or_newer_is_accepted() {
        for ok in [(2, 1), (2, 2), (2, 4), (3, 0)] {
            assert!(check_xi_version(ok.0, ok.1).is_ok(), "{ok:?}");
        }
        for old in [(2, 0), (1, 5), (0, 0)] {
            assert_eq!(
                check_xi_version(old.0, old.1),
                Err(InputError::Unsupported(XI_TOO_OLD)),
                "{old:?}"
            );
        }
    }

    #[test]
    fn keysyms_map_to_canonical_keys_with_sides() {
        assert_eq!(key_for_keysym(0xffea), Key::from_name("alt_r"));
        assert_eq!(key_for_keysym(0xffe9), Key::from_name("alt_l"));
        assert_eq!(key_for_keysym(0xffe4), Key::from_name("ctrl_r"));
        assert_eq!(key_for_keysym(0xffe3), Key::from_name("ctrl"));
        assert_eq!(key_for_keysym(0xffe2), Key::from_name("shift_r"));
        assert_eq!(key_for_keysym(0xffec), Key::from_name("cmd_r"));
        assert_eq!(key_for_keysym(0xffeb), Key::from_name("cmd"));
        assert_eq!(key_for_keysym(0xfe03), Key::from_name("alt_gr"));
        assert_eq!(key_for_keysym(XK_F1 + 18), Key::from_name("f19"));
        assert_eq!(key_for_keysym(XK_F1 + 19), Key::from_name("f20"));
        assert_eq!(key_for_keysym(XK_F1 + 20), None, "no f21 key name");
        assert_eq!(key_for_keysym(0x20), Key::from_name("space"));
        assert_eq!(key_for_keysym(0x64), Some(Key::Char('d')));
        assert_eq!(key_for_keysym(0x0100_00e9), Some(Key::Char('é')));
        assert_eq!(key_for_keysym(0), None);
        assert_ne!(key_for_keysym(0xffea), key_for_keysym(0xffe9));
    }

    #[test]
    fn repeat_is_suppressed_and_release_matches_the_press() {
        let mut t = Translator::default();
        assert_eq!(t.press(KEY_D, keymap(KEY_D)), vec![Key::Char('d')]);
        assert!(t.press(KEY_D, keymap(KEY_D)).is_empty(), "auto-repeat");
        assert_eq!(t.release(KEY_D, keymap(KEY_D)), vec![Key::Char('d')]);
        // Shift+= reports "+" too; releasing after Shift came up releases both.
        assert_eq!(
            t.press(SHIFT_L, keymap(SHIFT_L)),
            Key::from_name("shift").into_iter().collect::<Vec<_>>()
        );
        assert_eq!(
            t.press(KEY_EQUAL, keymap(KEY_EQUAL)),
            vec![Key::Char('='), Key::Char('+')]
        );
        t.release(SHIFT_L, keymap(SHIFT_L));
        assert_eq!(
            t.release(KEY_EQUAL, keymap(KEY_EQUAL)),
            vec![Key::Char('+'), Key::Char('=')]
        );
        // Unknown codes are swallowed (and their repeats too).
        assert!(t.press(999, [0, 0]).is_empty());
        assert!(t.release(999, [0, 0]).is_empty());
    }

    #[test]
    fn hold_to_talk_on_a_bare_right_modifier_and_left_side_does_not_trigger() {
        let (hub, log) = hub("alt_r", "", "");
        run(
            &hub,
            vec![
                RawKey::Press(ALT_L),
                RawKey::Release(ALT_L),
                RawKey::Press(ALT_R),
                RawKey::Press(ALT_R),
                RawKey::Press(ALT_R),
                RawKey::Release(ALT_R),
            ],
        )
        .unwrap();
        assert_eq!(log.take(), vec!["down", "up"], "repeat fires no extra down");
    }

    #[test]
    fn chords_match_either_side_and_toggle_fires_once_per_chord() {
        let (hub, log) = hub("", "<ctrl>+<alt>+d", "<ctrl>+<shift>+v");
        run(
            &hub,
            vec![
                RawKey::Press(CTRL_R),
                RawKey::Press(ALT_L),
                RawKey::Press(KEY_D),
                RawKey::Press(KEY_D),
                RawKey::Release(KEY_D),
                RawKey::Release(ALT_L),
                RawKey::Release(CTRL_R),
            ],
        )
        .unwrap();
        assert_eq!(log.take(), vec!["toggle"]);
    }

    #[test]
    fn stopping_or_a_backend_failure_releases_held_keys() {
        let (hub, log) = hub("f19", "", "");
        let stop = Arc::new(AtomicBool::new(false));
        let mut source = FakeSource {
            events: VecDeque::from(vec![
                Ok(Some(RawKey::Press(KEY_F19))),
                Err("X connection closed".into()),
            ]),
            stop_when_empty: None,
        };
        let error = pump(&mut source, &hub, &stop).unwrap_err();
        assert_eq!(error, InputError::Backend("X connection closed".into()));
        assert_eq!(
            log.take(),
            vec!["down", "up"],
            "a dead source ends the hold"
        );

        let (hub, log) = hub_with_hold("f19");
        let mut source = FakeSource {
            events: VecDeque::from(vec![Ok(Some(RawKey::Press(KEY_F19)))]),
            stop_when_empty: Some(stop.clone()),
        };
        stop.store(false, Ordering::Release);
        pump(&mut source, &hub, &stop).unwrap();
        assert_eq!(log.take(), vec!["down", "up"], "clean stop ends the hold");
    }

    fn hub_with_hold(key: &str) -> (KeyboardHub, Log) {
        hub(key, "", "")
    }

    #[test]
    fn a_stop_set_before_start_reads_nothing() {
        let (hub, log) = hub("f19", "", "");
        let stop = AtomicBool::new(true);
        let mut source = FakeSource {
            events: VecDeque::from(vec![Ok(Some(RawKey::Press(KEY_F19)))]),
            stop_when_empty: None,
        };
        pump(&mut source, &hub, &stop).unwrap();
        assert!(log.take().is_empty());
        assert_eq!(source.events.len(), 1);
    }

    #[test]
    fn release_of_a_key_pressed_before_the_listener_still_ends_a_hold() {
        let mut t = Translator::default();
        assert_eq!(
            t.release(ALT_R, keymap(ALT_R)),
            Key::from_name("alt_r").into_iter().collect::<Vec<_>>()
        );
        assert_eq!(
            t.press(SUPER_L, keymap(SUPER_L)),
            Key::from_name("cmd").into_iter().collect::<Vec<_>>()
        );
        assert_eq!(
            t.release_all(),
            Key::from_name("cmd").into_iter().collect::<Vec<_>>()
        );
        assert!(t.release_all().is_empty());
    }
}
