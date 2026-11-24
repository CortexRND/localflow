//! Listen-only Win32 keyboard hook for the current desktop.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WAIT_FAILED, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, MsgWaitForMultipleObjectsEx, PeekMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG,
    MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, WH_KEYBOARD_LL, WM_QUIT,
};

use crate::hub::KeyboardHub;
use crate::keys::{Key, Modifier, Side};
use crate::listener::InputError;

#[derive(Clone, Copy)]
struct KeyEvent {
    vk: u16,
    scan: u32,
    extended: bool,
    down: bool,
}

enum Event {
    Ready,
    Key(KeyEvent),
}

thread_local! {
    static EVENTS: RefCell<Option<SyncSender<Event>>> = const { RefCell::new(None) };
    static OVERRUN: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // WH_KEYBOARD_LL calls us on the installing thread. Hand off and return
    // immediately; Windows can silently remove a hook that takes too long.
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION Win32 provides a KBDLLHOOKSTRUCT for this call.
        let raw = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let extended = raw.flags & LLKHF_EXTENDED != 0;
        let vk = match raw.vkCode as u16 {
            VK_MENU if extended => VK_RMENU,
            VK_MENU => VK_LMENU,
            VK_CONTROL if extended => VK_RCONTROL,
            VK_CONTROL => VK_LCONTROL,
            VK_SHIFT if raw.scanCode == 0x36 => VK_RSHIFT,
            VK_SHIFT => VK_LSHIFT,
            vk => vk,
        };
        let _ = EVENTS.try_with(|events| {
            let Ok(events) = events.try_borrow() else {
                return;
            };
            let Some(events) = events.as_ref() else {
                return;
            };
            if events
                .try_send(Event::Key(KeyEvent {
                    vk,
                    scan: raw.scanCode,
                    extended,
                    down: raw.flags & LLKHF_UP == 0,
                }))
                .is_err()
            {
                let _ = OVERRUN.try_with(|overrun| overrun.set(true));
            }
        });
    }
    // SAFETY: forward the original hook arguments unchanged. No key is swallowed.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

struct Hook(HHOOK);

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: this thread owns the successfully installed hook.
        unsafe { UnhookWindowsHookEx(self.0) };
    }
}

fn os_error(operation: &str) -> InputError {
    InputError::Backend(format!("{operation}: {}", std::io::Error::last_os_error()))
}

fn hook_loop(
    events: SyncSender<Event>,
    stop: &AtomicBool,
    done: &AtomicBool,
) -> Result<(), InputError> {
    let mut message = MSG::default();
    // SAFETY: create the thread's message queue before exposing the hook.
    unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) };
    // SAFETY: the callback is in this executable and remains valid until
    // unhooked; thread id 0 observes input across the current desktop.
    let handle = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook),
            GetModuleHandleW(null()),
            0,
        )
    };
    if handle.is_null() {
        return Err(os_error("SetWindowsHookExW"));
    }
    let _hook = Hook(handle);
    // This is the first message; the hook thread has not pumped messages yet.
    if events.try_send(Event::Ready).is_err() {
        return Ok(());
    }
    EVENTS.with(|slot| slot.replace(Some(events)));
    while !stop.load(Ordering::Acquire) && !done.load(Ordering::Acquire) {
        // SAFETY: PeekMessageW pumps sent hook notifications on this thread.
        // It has no windows to dispatch messages to.
        let pending = unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
        if OVERRUN.with(Cell::get) {
            return Err(InputError::Backend(
                "Windows keyboard event queue closed or full".into(),
            ));
        }
        if pending != 0 {
            if message.message == WM_QUIT {
                break;
            }
            continue;
        }
        // SAFETY: zero kernel handles; wait only for messages, with a timeout
        // so a stop flag is noticed even when the keyboard is idle.
        let wait =
            unsafe { MsgWaitForMultipleObjectsEx(0, null(), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if wait == WAIT_FAILED {
            return Err(os_error("MsgWaitForMultipleObjectsEx"));
        }
    }
    Ok(())
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) fn run_hook(
    hub: &KeyboardHub,
    stop: &AtomicBool,
    ready: &dyn Fn(),
) -> Result<(), InputError> {
    let done = &AtomicBool::new(false);
    thread::scope(|scope| {
        let (send, receive) = mpsc::sync_channel(256);
        let hook = thread::Builder::new()
            .name("lf-win32-keyboard-hook".into())
            .spawn_scoped(scope, move || hook_loop(send, stop, done))
            .map_err(|e| {
                InputError::Backend(format!("could not start Windows keyboard hook: {e}"))
            })?;
        let cleanup = StopOnDrop(done);
        let mut translator = Translator::default();
        let mut registered = false;
        let mut next = None;
        while let Some(event) = next.take().or_else(|| receive.recv().ok()) {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match event {
                Event::Ready => {
                    registered = true;
                    ready();
                }
                Event::Key(event) => translator.feed(event, hub, &physically_down, &mut || {
                    next = next.take().or_else(|| receive.try_recv().ok());
                    next.is_none()
                }),
            }
        }
        drop(cleanup);
        let result = hook.join().unwrap_or_else(|_| {
            Err(InputError::Backend(
                "Windows keyboard hook thread panicked".into(),
            ))
        });
        if registered {
            hub.end_of_input();
        }
        result
    })
}

fn physically_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState only reads the system key state.
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    state as u16 & 0x8000 != 0
}

struct Pressed {
    vk: u16,
    keys: Vec<Key>,
}

#[derive(Default)]
struct Translator {
    pressed: HashMap<(u32, bool), Pressed>,
}

impl Translator {
    fn feed(
        &mut self,
        event: KeyEvent,
        hub: &KeyboardHub,
        physically_down: &dyn Fn(u16) -> bool,
        caught_up: &mut dyn FnMut() -> bool,
    ) {
        // Use the scan code when present: the same physical keypad key can
        // report different virtual keys if Num Lock changes mid-press.
        let code = if event.scan == 0 {
            u32::from(event.vk) | 0x8000_0000
        } else {
            event.scan
        };
        let id = (code, event.extended);
        // Key-ups on the secure desktop (Ctrl+Alt+Del, Win+L, UAC) never reach
        // the hook. The event being handled may not be reflected in the async
        // state yet, so only the other keys are checked. The hook queues every
        // event, down or up, before the async state changes, so a key that
        // reads as up while nothing is queued (checked after the read) lost its
        // key-up; with events queued, they come in order. Checking on key-ups
        // too lets the last event of a burst repair what was missed.
        let mut missed: Vec<_> = self
            .pressed
            .iter()
            .filter(|(other, pressed)| **other != id && !physically_down(pressed.vk))
            .map(|(other, _)| *other)
            .collect();
        if !missed.is_empty() && caught_up() {
            missed.sort_unstable();
            for other in missed {
                self.release(other, hub);
            }
        }
        if event.down {
            if self.pressed.contains_key(&id) {
                return; // auto-repeat
            }
            if let Some((base, shifted)) = keys_for_vk(event.vk) {
                let shift = self.is_pressed(&Key::Mod(Modifier::Shift, Side::Left))
                    || self.is_pressed(&Key::Mod(Modifier::Shift, Side::Right));
                let mut keys = vec![base];
                if let Some(ch) = shifted.filter(|_| shift) {
                    keys.push(Key::Char(ch));
                }
                for key in &keys {
                    if !self.is_pressed(key) {
                        hub.press(key.clone());
                    }
                }
                self.pressed.insert(id, Pressed { vk: event.vk, keys });
            }
        } else {
            self.release(id, hub);
        }
    }

    fn release(&mut self, id: (u32, bool), hub: &KeyboardHub) {
        let Some(pressed) = self.pressed.remove(&id) else {
            return;
        };
        // Release exactly what key-down reported, even if Shift came up first.
        for key in pressed.keys.into_iter().rev() {
            if !self.is_pressed(&key) {
                hub.release(key);
            }
        }
    }

    fn is_pressed(&self, key: &Key) -> bool {
        self.pressed
            .values()
            .any(|pressed| pressed.keys.contains(key))
    }
}

fn keys_for_vk(vk: u16) -> Option<(Key, Option<char>)> {
    let named = |name| (Key::Named(name), None);
    let modifier = |modifier, side| (Key::Mod(modifier, side), None);
    let printable = |base, shifted| (Key::Char(base), Some(shifted));
    Some(match vk {
        VK_LSHIFT => modifier(Modifier::Shift, Side::Left),
        VK_RSHIFT => modifier(Modifier::Shift, Side::Right),
        VK_LCONTROL => modifier(Modifier::Ctrl, Side::Left),
        VK_RCONTROL => modifier(Modifier::Ctrl, Side::Right),
        VK_LMENU => modifier(Modifier::Alt, Side::Left),
        VK_RMENU => modifier(Modifier::Alt, Side::Right),
        VK_LWIN => modifier(Modifier::Cmd, Side::Left),
        VK_RWIN => modifier(Modifier::Cmd, Side::Right),
        VK_BACK => named("backspace"),
        VK_CAPITAL => named("caps_lock"),
        VK_DELETE => named("delete"),
        VK_DOWN => named("down"),
        VK_END => named("end"),
        VK_RETURN => named("enter"),
        VK_ESCAPE => named("esc"),
        VK_F1..=VK_F20 => {
            return Key::from_name(&format!("f{}", vk - VK_F1 + 1)).map(|key| (key, None))
        }
        VK_HOME => named("home"),
        VK_INSERT => named("insert"),
        VK_LEFT => named("left"),
        VK_APPS => named("menu"),
        VK_NUMLOCK => named("num_lock"),
        VK_NEXT => named("page_down"),
        VK_PRIOR => named("page_up"),
        VK_PAUSE => named("pause"),
        VK_SNAPSHOT => named("print_screen"),
        VK_RIGHT => named("right"),
        VK_SCROLL => named("scroll_lock"),
        VK_SPACE => named("space"),
        VK_TAB => named("tab"),
        VK_UP => named("up"),
        // Use virtual keys, not typed text; punctuation uses the US VK names.
        VK_A..=VK_Z => (Key::Char(char::from(b'a' + (vk - VK_A) as u8)), None),
        VK_0..=VK_9 => printable(
            vk as u8 as char,
            b")!@#$%^&*("[(vk - VK_0) as usize] as char,
        ),
        VK_NUMPAD0..=VK_NUMPAD9 => (Key::Char(char::from(b'0' + (vk - VK_NUMPAD0) as u8)), None),
        VK_ADD => (Key::Char('+'), None),
        VK_SUBTRACT => (Key::Char('-'), None),
        VK_MULTIPLY => (Key::Char('*'), None),
        VK_DIVIDE => (Key::Char('/'), None),
        VK_DECIMAL => (Key::Char('.'), None),
        VK_OEM_1 => printable(';', ':'),
        VK_OEM_PLUS => printable('=', '+'),
        VK_OEM_COMMA => printable(',', '<'),
        VK_OEM_MINUS => printable('-', '_'),
        VK_OEM_PERIOD => printable('.', '>'),
        VK_OEM_2 => printable('/', '?'),
        VK_OEM_3 => printable('`', '~'),
        VK_OEM_4 => printable('[', '{'),
        VK_OEM_5 => printable('\\', '|'),
        VK_OEM_6 => printable(']', '}'),
        VK_OEM_7 => printable('\'', '"'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{HubActions, Shortcuts};

    #[test]
    fn virtual_keys_keep_modifier_sides_and_pynput_names() {
        for (vk, name) in [
            (VK_LMENU, "alt_l"),
            (VK_RMENU, "alt_r"),
            (VK_RMENU, "alt_gr"),
            (VK_LSHIFT, "shift_l"),
            (VK_RCONTROL, "ctrl_r"),
            (VK_RWIN, "cmd_r"),
            (VK_F20, "f20"),
            (VK_ESCAPE, "esc"),
            (VK_A, "a"),
        ] {
            assert_eq!(keys_for_vk(vk).map(|(base, _)| base), Key::from_name(name));
        }
        assert_eq!(keys_for_vk(VK_PACKET), None);
    }

    #[test]
    fn releasing_shift_first_still_releases_the_shifted_chord() {
        let (tx, rx) = mpsc::channel();
        let (hub, warnings) = KeyboardHub::new(
            &Shortcuts {
                toggle_combo: "<ctrl>++".into(),
                ..Default::default()
            },
            HubActions {
                on_toggle: Arc::new(move || tx.send(()).unwrap()),
                ..HubActions::noop()
            },
        );
        assert!(warnings.is_empty());
        let mut translator = Translator::default();
        let mut feed = |vk, down| {
            translator.feed(
                KeyEvent {
                    vk,
                    scan: 0,
                    extended: false,
                    down,
                },
                &hub,
                &|_| true,
                &mut || true,
            );
        };
        feed(VK_LCONTROL, true);
        for _ in 0..2 {
            feed(VK_RSHIFT, true);
            feed(VK_OEM_PLUS, true);
            feed(VK_OEM_PLUS, true); // repeat
            feed(VK_RSHIFT, false);
            feed(VK_OEM_PLUS, false);
            assert_eq!(rx.try_recv(), Ok(()));
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn a_key_up_missed_on_the_secure_desktop_is_released_on_the_next_key_down() {
        let (tx, rx) = mpsc::channel();
        let (down, up) = (tx.clone(), tx);
        let (hub, warnings) = KeyboardHub::new(
            &Shortcuts {
                hold_key: "alt_l".into(),
                ..Default::default()
            },
            HubActions {
                on_down: Arc::new(move || down.send("down").unwrap()),
                on_up: Arc::new(move || up.send("up").unwrap()),
                ..HubActions::noop()
            },
        );
        assert!(warnings.is_empty());
        let mut translator = Translator::default();
        let held = RefCell::new(Vec::new());
        let mut feed = |vk, down| {
            if down {
                held.borrow_mut().push(vk);
            }
            translator.feed(
                KeyEvent {
                    vk,
                    scan: 0,
                    extended: false,
                    down,
                },
                &hub,
                &|vk| held.borrow().contains(&vk),
                &mut || true,
            );
        };
        feed(VK_LCONTROL, true);
        feed(VK_LMENU, true);
        feed(VK_DELETE, true);
        assert_eq!(rx.try_recv(), Ok("down"));
        feed(VK_LMENU, true); // auto-repeat while all three are still down
        assert!(rx.try_recv().is_err());
        held.borrow_mut().clear(); // the ups go to the secure desktop
        feed(VK_A, true);
        assert_eq!(rx.try_recv(), Ok("up"));
        held.borrow_mut().clear();
        feed(VK_A, false);
        feed(VK_LMENU, true);
        assert_eq!(rx.try_recv(), Ok("down"));
        feed(VK_LMENU, false);
        assert_eq!(rx.try_recv(), Ok("up"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_missed_key_up_is_released_on_a_later_key_up_once_caught_up() {
        let (tx, rx) = mpsc::channel();
        let (down, up) = (tx.clone(), tx);
        let (hub, warnings) = KeyboardHub::new(
            &Shortcuts {
                hold_key: "alt_l".into(),
                ..Default::default()
            },
            HubActions {
                on_down: Arc::new(move || down.send("down").unwrap()),
                on_up: Arc::new(move || up.send("up").unwrap()),
                ..HubActions::noop()
            },
        );
        assert!(warnings.is_empty());
        let mut translator = Translator::default();
        let event = |vk, down| KeyEvent {
            vk,
            scan: 0,
            extended: false,
            down,
        };
        translator.feed(event(VK_LMENU, true), &hub, &|_| true, &mut || true);
        assert_eq!(rx.try_recv(), Ok("down"));
        // Alt's key-up went to the secure desktop; A's key-up is still queued.
        translator.feed(event(VK_A, true), &hub, &|vk| vk == VK_A, &mut || false);
        assert!(rx.try_recv().is_err());
        translator.feed(event(VK_A, false), &hub, &|_| false, &mut || true);
        assert_eq!(rx.try_recv(), Ok("up"));
        assert!(rx.try_recv().is_err());
        assert!(translator.pressed.is_empty());
    }

    #[test]
    fn a_fast_chord_still_queued_is_not_released_early() {
        let (tx, rx) = mpsc::channel();
        let (hub, warnings) = KeyboardHub::new(
            &Shortcuts {
                toggle_combo: "<ctrl>+x".into(),
                ..Default::default()
            },
            HubActions {
                on_toggle: Arc::new(move || tx.send(()).unwrap()),
                ..HubActions::noop()
            },
        );
        assert!(warnings.is_empty());
        let mut translator = Translator::default();
        // Ctrl down, X down, X up, Ctrl up were queued; both keys are
        // already physically up while the translator handles X down.
        for (vk, down) in [
            (VK_LCONTROL, true),
            (VK_X, true),
            (VK_X, false),
            (VK_LCONTROL, false),
        ] {
            let event = KeyEvent {
                vk,
                scan: 0,
                extended: false,
                down,
            };
            translator.feed(event, &hub, &|_| false, &mut || false);
        }
        assert_eq!(rx.try_recv(), Ok(()));
        assert!(rx.try_recv().is_err());
        assert!(translator.pressed.is_empty());
    }
}
