//! macOS: CGEventTap keyboard listener, TCC checks and CGEvent keystrokes.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventType, CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use crate::hub::KeyboardHub;
use crate::keys::{Key, Modifier, Side};
use crate::listener::{emit, watchdog_armed, InputError, InputWarning, WarningSink, DEAD_TAP, LISTEN_EVENT};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> i32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn AXIsProcessTrusted() -> bool;
}

pub fn input_monitoring_granted() -> bool {
    // kIOHIDAccessTypeGranted == 0; "unknown" is not a grant.
    unsafe { IOHIDCheckAccess(LISTEN_EVENT) == 0 }
}

fn request_input_monitoring() {
    unsafe {
        IOHIDRequestAccess(LISTEN_EVENT);
    }
}

/// Accessibility permission, needed to post synthetic Cmd+V.
pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Virtual keycode → pynput key, by US layout position so Option doesn't
/// turn "2" into "™".
pub fn key_for_code(code: u16) -> Option<Key> {
    let m = |m, s| Some(Key::Mod(m, s));
    let named = |n: &'static str| Some(Key::Named(n));
    const CHARS: &[(u16, char)] = &[
        (0, 'a'), (1, 's'), (2, 'd'), (3, 'f'), (4, 'h'), (5, 'g'), (6, 'z'), (7, 'x'),
        (8, 'c'), (9, 'v'), (11, 'b'), (12, 'q'), (13, 'w'), (14, 'e'), (15, 'r'),
        (16, 'y'), (17, 't'), (18, '1'), (19, '2'), (20, '3'), (21, '4'), (22, '6'),
        (23, '5'), (24, '='), (25, '9'), (26, '7'), (27, '-'), (28, '8'), (29, '0'),
        (30, ']'), (31, 'o'), (32, 'u'), (33, '['), (34, 'i'), (35, 'p'), (37, 'l'),
        (38, 'j'), (39, '\''), (40, 'k'), (41, ';'), (42, '\\'), (43, ','), (44, '/'),
        (45, 'n'), (46, 'm'), (47, '.'), (50, '`'),
    ];
    if let Some((_, c)) = CHARS.iter().find(|(k, _)| *k == code) {
        return Some(Key::Char(*c));
    }
    match code {
        36 => named("enter"),
        48 => named("tab"),
        49 => named("space"),
        51 => named("backspace"),
        53 => named("esc"),
        54 => m(Modifier::Cmd, Side::Right),
        55 => m(Modifier::Cmd, Side::Left),
        56 => m(Modifier::Shift, Side::Left),
        57 => named("caps_lock"),
        58 => m(Modifier::Alt, Side::Left),
        59 => m(Modifier::Ctrl, Side::Left),
        60 => m(Modifier::Shift, Side::Right),
        61 => m(Modifier::Alt, Side::Right),
        62 => m(Modifier::Ctrl, Side::Right),
        64 => named("f17"),
        79 => named("f18"),
        80 => named("f19"),
        90 => named("f20"),
        96 => named("f5"),
        97 => named("f6"),
        98 => named("f7"),
        99 => named("f3"),
        100 => named("f8"),
        101 => named("f9"),
        103 => named("f11"),
        105 => named("f13"),
        106 => named("f16"),
        107 => named("f14"),
        109 => named("f10"),
        111 => named("f12"),
        113 => named("f15"),
        114 => named("insert"),
        115 => named("home"),
        116 => named("page_up"),
        117 => named("delete"),
        118 => named("f4"),
        119 => named("end"),
        120 => named("f2"),
        121 => named("page_down"),
        122 => named("f1"),
        123 => named("left"),
        124 => named("right"),
        125 => named("down"),
        126 => named("up"),
        _ => None,
    }
}

/// Device-dependent flag bit (NX_DEVICE*KEYMASK) telling a modifier's state.
fn modifier_mask(code: u16) -> Option<u64> {
    Some(match code {
        59 => 0x0000_0001,
        56 => 0x0000_0002,
        60 => 0x0000_0004,
        55 => 0x0000_0008,
        54 => 0x0000_0010,
        58 => 0x0000_0020,
        61 => 0x0000_0040,
        62 => 0x0000_2000,
        57 => 0x0001_0000,
        _ => return None,
    })
}

pub fn run_tap(hub: &KeyboardHub, warn: WarningSink) -> Result<(), InputError> {
    let granted = input_monitoring_granted();
    if !granted {
        request_input_monitoring();
        emit(&warn, InputWarning::InputMonitoringDenied);
    }
    let port = Arc::new(AtomicUsize::new(0));
    let tap_port = port.clone();
    let feed = hub.clone();
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::KeyDown, CGEventType::KeyUp, CGEventType::FlagsChanged],
        move |_proxy, etype, event: &CGEvent| {
            match etype {
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    let raw = tap_port.load(Ordering::SeqCst);
                    if raw != 0 {
                        unsafe { CGEventTapEnable(raw as *mut c_void, true) };
                    }
                }
                CGEventType::KeyDown | CGEventType::KeyUp => {
                    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let repeat = event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;
                    if let Some(key) = key_for_code(code) {
                        if matches!(etype, CGEventType::KeyDown) {
                            if !repeat {
                                feed.press(key);
                            }
                        } else {
                            feed.release(key);
                        }
                    }
                }
                CGEventType::FlagsChanged => {
                    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    if let (Some(key), Some(mask)) = (key_for_code(code), modifier_mask(code)) {
                        if event.get_flags().bits() & mask != 0 {
                            feed.press(key);
                        } else {
                            feed.release(key);
                        }
                    }
                }
                _ => {}
            }
            CallbackResult::Keep
        },
    )
    .map_err(|()| {
        emit(&warn, InputWarning::InputMonitoringDenied);
        InputError::TapCreateFailed
    })?;
    {
        use core_foundation::base::TCFType;
        port.store(tap.mach_port().as_concrete_TypeRef() as usize, Ordering::SeqCst);
    }
    let source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|()| InputError::TapCreateFailed)?;
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    if watchdog_armed(granted) {
        let hub = hub.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEAD_TAP);
            crate::listener::check_tap_alive(&hub, &warn);
        });
    }
    CFRunLoop::run_current();
    log::warn!("keyboard listener exited");
    Ok(())
}

/// Post Cmd+<keycode> through the HID tap (paste = 9 "v", copy = 8 "c").
pub fn post_command_key(code: u16) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|()| "CGEventSource unavailable".to_string())?;
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(source.clone(), code, down)
            .map_err(|()| "CGEvent unavailable".to_string())?;
        event.set_flags(CGEventFlags::CGEventFlagCommand);
        event.post(CGEventTapLocation::HID);
    }
    Ok(())
}
