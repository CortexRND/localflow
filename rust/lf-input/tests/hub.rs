//! Mirrors tests/test_hotkey.py and tests/test_review_29_shortcuts.py.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lf_input::hub::{BindingWarning, Callback, HubActions, KeyboardHub, Shortcuts};
use lf_input::{keys_for_typed, Key};

type Log = Arc<Mutex<Vec<&'static str>>>;

fn rec(log: &Log, name: &'static str) -> Callback {
    let log = log.clone();
    Arc::new(move || log.lock().unwrap().push(name))
}

fn actions(log: &Log) -> HubActions {
    HubActions {
        on_down: rec(log, "dictation-down"),
        on_up: rec(log, "dictation-up"),
        on_toggle: rec(log, "toggle"),
        on_cancel: rec(log, "cancel"),
        on_command_down: rec(log, "command-down"),
        on_command_up: rec(log, "command-up"),
        on_command_cancel: rec(log, "command-cancel"),
        can_reconfigure: Arc::new(|| true),
    }
}

fn hub(shortcuts: Shortcuts, log: &Log) -> (KeyboardHub, Vec<BindingWarning>) {
    // A long command delay so only fire_command_timer() fires it.
    KeyboardHub::with_options(
        &shortcuts,
        actions(log),
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    )
}

fn k(name: &str) -> Key {
    Key::from_name(name).unwrap()
}

fn tap_chord(hub: &KeyboardHub, keys: &[&str]) {
    for key in keys {
        hub.press(k(key));
    }
    for key in keys.iter().rev() {
        hub.release(k(key));
    }
}

#[test]
fn routes_multiple_combos_and_escape() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        combos: vec![
            ("<ctrl>+<alt>+1".into(), rec(&log, "polish")),
            ("<ctrl>+<alt>+2".into(), rec(&log, "prompt")),
        ],
        ..Default::default()
    };
    let (hub, warnings) = hub(shortcuts, &log);
    assert!(warnings.is_empty());
    tap_chord(&hub, &["ctrl", "alt", "1"]);
    tap_chord(&hub, &["ctrl_l", "alt_r", "2"]);
    hub.press(k("esc"));
    assert_eq!(*log.lock().unwrap(), vec!["polish", "prompt", "cancel"]);
}

#[test]
fn routes_hold_and_command_key_events() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        hold_key: "f19".into(),
        command_key: "alt_r".into(),
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    hub.press(k("f19"));
    hub.release(k("f19"));
    hub.press(k("alt_r"));
    hub.fire_command_timer();
    hub.release(k("alt_r"));
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "dictation-down",
            "dictation-up",
            "command-down",
            "command-up"
        ]
    );
}

#[test]
fn end_of_input_ends_a_hold_but_cancels_an_unfinished_command() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        hold_key: "f19".into(),
        command_key: "alt_r".into(),
        toggle_combo: "<ctrl>+<alt>+t".into(),
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    // Command mode entered, key still held, then the source is gone.
    hub.press(k("alt_r"));
    hub.fire_command_timer();
    hub.end_of_input();
    assert_eq!(
        *log.lock().unwrap(),
        vec!["command-down", "command-cancel"],
        "never command-up"
    );
    // Idempotent; a later physical key-up of that key submits nothing.
    hub.end_of_input();
    hub.release(k("alt_r"));
    assert_eq!(log.lock().unwrap().len(), 2);

    // Command key pressed but timer not fired yet: disarmed, nothing fires.
    log.lock().unwrap().clear();
    hub.press(k("alt_r"));
    hub.end_of_input();
    hub.fire_command_timer();
    assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());

    // A held dictation key still ends normally.
    hub.press(k("f19"));
    hub.end_of_input();
    assert_eq!(*log.lock().unwrap(), vec!["dictation-down", "dictation-up"]);

    // Partial chords are forgotten: no stale toggle completion afterwards.
    log.lock().unwrap().clear();
    hub.press(k("ctrl"));
    hub.press(k("alt"));
    hub.end_of_input();
    hub.press(k("t"));
    assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
    // ...while a full fresh chord still works.
    hub.release(k("t"));
    tap_chord(&hub, &["ctrl", "alt", "t"]);
    assert_eq!(*log.lock().unwrap(), vec!["toggle"]);
}

#[test]
fn release_racing_the_command_timer_is_delivered_after_command_down() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        command_key: "alt_r".into(),
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    hub.press(k("alt_r"));
    // The key comes up on the listener thread after the timer decided to
    // enter command mode but before it delivered command-down.
    hub.fire_command_timer_with_gap(|| {
        let listener = hub.clone();
        std::thread::spawn(move || listener.release(k("alt_r")))
            .join()
            .unwrap();
    });
    assert_eq!(*log.lock().unwrap(), vec!["command-down", "command-up"]);
}

#[test]
fn a_callback_re_entering_the_hub_does_not_deadlock() {
    let log: Log = Default::default();
    let slot: Arc<std::sync::OnceLock<KeyboardHub>> = Default::default();
    let reenter = {
        let slot = slot.clone();
        let log = log.clone();
        Arc::new(move || {
            log.lock().unwrap().push("toggle");
            let hub = slot.get().unwrap();
            hub.press(k("f19"));
            hub.release(k("f19"));
            log.lock().unwrap().push("toggle-done");
        }) as Callback
    };
    let shortcuts = Shortcuts {
        hold_key: "f19".into(),
        toggle_combo: "<ctrl>+<alt>+t".into(),
        ..Default::default()
    };
    let mut acts = actions(&log);
    acts.on_toggle = reenter;
    let (hub, _) = KeyboardHub::with_options(
        &shortcuts,
        acts,
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    );
    slot.set(hub.clone()).ok().unwrap();
    tap_chord(&hub, &["ctrl_l", "alt_l", "t"]);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["toggle", "toggle-done", "dictation-down", "dictation-up"]
    );
}

#[test]
fn command_timer_thread_fires_without_polling() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        command_key: "alt_r".into(),
        ..Default::default()
    };
    let (hub, _) = KeyboardHub::with_options(
        &shortcuts,
        actions(&log),
        Arc::new(|| 0.0),
        Duration::from_millis(20),
    );
    hub.press(k("alt_r"));
    for _ in 0..200 {
        if !log.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    hub.release(k("alt_r"));
    assert_eq!(*log.lock().unwrap(), vec!["command-down", "command-up"]);
}

#[test]
fn quick_command_tap_never_enters_command_mode() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        command_key: "alt_r".into(),
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    hub.press(k("alt_r"));
    hub.release(k("alt_r"));
    hub.fire_command_timer();
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn disables_later_duplicate_shortcuts() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        hold_key: "f19".into(),
        combos: vec![("f19".into(), rec(&log, "combo"))],
        command_key: "f19".into(),
        ..Default::default()
    };
    let (hub, warnings) = hub(shortcuts, &log);
    hub.press(k("f19"));
    assert_eq!(*log.lock().unwrap(), vec!["dictation-down"]);
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|w| w.to_string().contains("disabling")));
}

#[test]
fn reordered_modifiers_are_the_same_chord_and_rejected() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        combos: vec![
            ("Option+Control+2".into(), rec(&log, "polish")),
            ("Control+Option+2".into(), rec(&log, "prompt")),
            ("<ctrl>+<alt>+2".into(), rec(&log, "third")),
        ],
        ..Default::default()
    };
    let (hub, warnings) = hub(shortcuts, &log);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    tap_chord(&hub, &["alt", "ctrl", "2"]);
    assert_eq!(*log.lock().unwrap(), vec!["polish"]);
}

#[test]
fn toggle_combo_clashes_with_a_reordered_combo() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        toggle_combo: "<cmd>+<shift>+d".into(),
        combos: vec![("<shift>+<cmd>+D".into(), rec(&log, "polish"))],
        ..Default::default()
    };
    let (hub, warnings) = hub(shortcuts, &log);
    assert_eq!(warnings.len(), 1);
    tap_chord(&hub, &["cmd", "shift", "d"]);
    assert_eq!(*log.lock().unwrap(), vec!["toggle"]);
}

#[test]
fn distinct_chords_are_still_allowed() {
    for (left, right) in [
        ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<shift>+1"),
        ("<ctrl>+<alt>+1", "<cmd>+<alt>+1"),
        ("<ctrl>+<alt>+1", "<ctrl>+<alt>+<f1>"),
        ("<ctrl>+<alt>+<up>", "<ctrl>+<alt>+<down>"),
    ] {
        let log: Log = Default::default();
        let shortcuts = Shortcuts {
            combos: vec![
                (left.into(), rec(&log, "a")),
                (right.into(), rec(&log, "b")),
            ],
            ..Default::default()
        };
        assert!(hub(shortcuts, &log).1.is_empty(), "{left} vs {right}");
    }
}

#[test]
fn hold_sides_are_distinct_but_identical_holds_clash() {
    let log: Log = Default::default();
    let distinct = Shortcuts {
        hold_key: "alt_l".into(),
        command_key: "alt_r".into(),
        ..Default::default()
    };
    assert!(hub(distinct, &log).1.is_empty());
    let same = Shortcuts {
        hold_key: "alt".into(),
        command_key: "alt_l".into(),
        ..Default::default()
    };
    assert_eq!(hub(same, &log).1.len(), 1);
    let hold_vs_combo = Shortcuts {
        command_key: "alt_r".into(),
        combos: vec![("<alt>+<ctrl>+1".into(), rec(&log, "x"))],
        ..Default::default()
    };
    assert!(hub(hold_vs_combo, &log).1.is_empty());
}

#[test]
fn command_and_hold_modifier_sides_are_distinct() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        hold_key: "alt_l".into(),
        command_key: "alt_r".into(),
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    hub.press(k("alt_l"));
    hub.release(k("alt_l"));
    hub.press(k("alt_r"));
    hub.release(k("alt_r"));
    hub.fire_command_timer();
    assert_eq!(*log.lock().unwrap(), vec!["dictation-down", "dictation-up"]);
}

#[test]
fn command_modifier_in_transform_chord_does_not_start_command() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        command_key: "alt_r".into(),
        combos: vec![("<ctrl>+<alt>+1".into(), rec(&log, "polish"))],
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    for key in ["ctrl", "alt_r", "1"] {
        hub.press(k(key));
    }
    hub.fire_command_timer();
    assert_eq!(*log.lock().unwrap(), vec!["polish"]);
}

#[test]
fn active_command_is_cancelled_before_command_key_chord_callback() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        command_key: "alt_r".into(),
        combos: vec![("<ctrl>+<alt>+1".into(), rec(&log, "polish"))],
        ..Default::default()
    };
    let (hub, _) = hub(shortcuts, &log);
    hub.press(k("alt_r"));
    hub.fire_command_timer();
    hub.press(k("ctrl"));
    hub.press(k("1"));
    hub.release(k("alt_r"));
    assert_eq!(
        *log.lock().unwrap(),
        vec!["command-down", "command-cancel", "polish"]
    );
}

#[test]
fn reconfigure_is_deferred_until_keys_are_up() {
    let log: Log = Default::default();
    let first = Shortcuts {
        hold_key: "f19".into(),
        ..Default::default()
    };
    let (hub, _) = hub(first, &log);
    hub.press(k("f19"));
    let second = Shortcuts {
        hold_key: "f18".into(),
        ..Default::default()
    };
    assert!(!hub.reconfigure(&second).0);
    assert!(hub.reconfigure_pending());
    // The gesture started with f19 ends with f19.
    hub.release(k("f19"));
    assert!(!hub.reconfigure_pending());
    hub.press(k("f18"));
    hub.release(k("f18"));
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "dictation-down",
            "dictation-up",
            "dictation-down",
            "dictation-up"
        ]
    );
}

#[test]
fn reconfigure_waits_for_can_reconfigure() {
    let log: Log = Default::default();
    let busy = Arc::new(AtomicBool::new(true));
    let mut acts = actions(&log);
    let flag = busy.clone();
    acts.can_reconfigure = Arc::new(move || !flag.load(Ordering::SeqCst));
    let (hub, _) = KeyboardHub::with_options(
        &Shortcuts::default(),
        acts,
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    );
    let next = Shortcuts {
        hold_key: "f18".into(),
        ..Default::default()
    };
    assert!(!hub.reconfigure(&next).0);
    busy.store(false, Ordering::SeqCst);
    assert!(hub.apply_pending());
}

#[test]
fn stale_press_does_not_block_reconfigure_forever() {
    let log: Log = Default::default();
    let now = Arc::new(Mutex::new(0.0f64));
    let clock_now = now.clone();
    let (hub, _) = KeyboardHub::with_options(
        &Shortcuts::default(),
        actions(&log),
        Arc::new(move || *clock_now.lock().unwrap()),
        Duration::from_secs(3600),
    );
    hub.press(k("a"));
    let next = Shortcuts {
        hold_key: "f18".into(),
        ..Default::default()
    };
    assert!(!hub.reconfigure(&next).0);
    *now.lock().unwrap() = 10.5;
    assert!(hub.apply_pending());
}

#[test]
fn first_event_marks_tap_alive() {
    let log: Log = Default::default();
    let (hub, _) = hub(Shortcuts::default(), &log);
    let warned: Arc<Mutex<Vec<lf_input::InputWarning>>> = Default::default();
    let sink_log = warned.clone();
    let sink: lf_input::listener::WarningSink = Arc::new(move |w| sink_log.lock().unwrap().push(w));
    assert!(!lf_input::listener::check_tap_alive(&hub, &sink));
    hub.press(k("a"));
    assert!(lf_input::listener::check_tap_alive(&hub, &sink));
    assert_eq!(
        *warned.lock().unwrap(),
        vec![lf_input::InputWarning::DeadTap]
    );
    assert!(lf_input::listener::watchdog_armed(false));
    assert!(!lf_input::listener::watchdog_armed(true));
}

#[test]
fn shift_adds_the_typed_character_like_pynput() {
    let eq = Key::Char('=');
    assert_eq!(
        keys_for_typed(eq.clone(), true, false, "+"),
        vec![eq.clone(), Key::Char('+')]
    );
    assert_eq!(
        keys_for_typed(eq.clone(), false, false, "="),
        vec![eq.clone()]
    );
    // Option would type "≠"; keycode position wins, as for Option+2.
    assert_eq!(
        keys_for_typed(eq.clone(), true, true, "±"),
        vec![eq.clone()]
    );
    // Same key after case folding, control characters and named keys add nothing.
    assert_eq!(
        keys_for_typed(Key::Char('a'), true, false, "A"),
        vec![Key::Char('a')]
    );
    assert_eq!(
        keys_for_typed(eq.clone(), true, false, "\u{1d}"),
        vec![eq.clone()]
    );
    assert_eq!(keys_for_typed(eq.clone(), true, false, ""), vec![eq]);
    assert_eq!(keys_for_typed(k("tab"), true, false, "\t"), vec![k("tab")]);
}

#[test]
fn ctrl_plus_fires_on_ctrl_shift_equal_and_shift_chords_still_work() {
    let log: Log = Default::default();
    let shortcuts = Shortcuts {
        combos: vec![
            ("<ctrl>++".into(), rec(&log, "plus")),
            ("<ctrl>+<shift>+2".into(), rec(&log, "shift-2")),
        ],
        ..Default::default()
    };
    let (hub, warnings) = hub(shortcuts, &log);
    assert!(warnings.is_empty());
    let press_typed = |base: &str, typed: &str| {
        let keys = keys_for_typed(k(base), true, false, typed);
        for key in &keys {
            hub.press(key.clone());
        }
        keys
    };
    hub.press(k("ctrl_l"));
    hub.press(k("shift_l"));
    let plus = press_typed("=", "+");
    hub.release(k("shift_l"));
    for key in plus.into_iter().rev() {
        hub.release(key);
    }
    hub.press(k("shift_l"));
    let at = press_typed("2", "@");
    for key in at.into_iter().rev() {
        hub.release(key);
    }
    hub.release(k("shift_l"));
    hub.release(k("ctrl_l"));
    assert_eq!(*log.lock().unwrap(), vec!["plus", "shift-2"]);
}
