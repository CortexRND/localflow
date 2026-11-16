//! Mirrors tests/test_pushtotalk.py and tests/test_dictation_toggle.py.

mod common;

use std::sync::Arc;

use common::{config, rig, rig_with};
use lf_runtime::controller::{Event, Gesture, JobKind};

#[test]
fn rapid_start_stop_pair_is_coalesced_without_touching_mic() {
    let mut r = rig();
    r.handle.send(Event::Start(0.0));
    r.handle.send(Event::Stop(0.1));
    r.controller.drain();
    assert_eq!(r.start_calls(), 0);
    assert_eq!(r.jobs(), 0);
}

#[test]
fn normal_hold_above_threshold_records_and_queues_clip() {
    let mut r = rig();
    r.handle.send(Event::Down(0.0));
    r.controller.drain();
    assert!(r.recording());
    assert_eq!(r.controller.gesture(), Gesture::Held);
    r.set_now(1.0);
    r.handle.send(Event::Up(1.0));
    r.controller.drain();
    assert!(!r.recording());
    assert_eq!(r.jobs(), 1);
    assert_eq!(r.jobs.lock().unwrap()[0].kind, JobKind::Dictate);
    assert_eq!(*r.states.lock().unwrap(), vec!["recording", "processing"]);
    assert_eq!(*r.sounds.lock().unwrap(), vec!["start", "stop"]);
}

#[test]
fn queued_short_tap_pair_never_opens_microphone() {
    let mut r = rig();
    r.handle.send(Event::Down(0.0));
    r.handle.send(Event::Up(0.1));
    r.controller.drain();
    assert_eq!(r.start_calls(), 0);
    assert_eq!(r.controller.gesture(), Gesture::Tapped);
    // The double-tap window expires: back to idle, still no mic.
    r.set_now(0.6);
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Idle);
    assert_eq!(r.start_calls(), 0);
}

#[test]
fn short_tap_locks_on_second_down_and_triple_tap_cancels() {
    let mut r = rig();
    for (event, now) in [(Event::Down(0.0), 0.0), (Event::Up(0.1), 0.1)] {
        r.set_now(now);
        r.handle.send(event);
        r.controller.drain();
    }
    assert_eq!(r.controller.gesture(), Gesture::Tapped);
    r.set_now(0.3);
    r.handle.send(Event::Down(0.3));
    r.controller.drain();
    r.handle.send(Event::Up(0.31));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Locked);
    assert!(r.recording());
    assert_eq!(*r.sounds.lock().unwrap(), vec!["start", "lock"]);
    r.set_now(0.6);
    r.handle.send(Event::Down(0.6));
    r.controller.drain();
    assert!(!r.recording());
    assert_eq!(r.controller.gesture(), Gesture::AwaitUp);
    assert_eq!(r.jobs(), 0);
    assert_eq!(r.sounds.lock().unwrap().last().unwrap(), "cancel");
    r.handle.send(Event::Up(0.61));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Idle);
}

#[test]
fn locked_recording_stops_on_a_later_press() {
    let mut r = rig();
    r.handle.send(Event::Down(0.0));
    r.controller.drain();
    r.handle.send(Event::Up(0.1));
    r.controller.drain();
    r.handle.send(Event::Down(0.3));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Locked);
    r.set_now(5.0);
    r.handle.send(Event::Down(5.0));
    r.controller.drain();
    assert_eq!(r.jobs(), 1);
    assert_eq!(r.controller.gesture(), Gesture::AwaitUp);
}

#[test]
fn escape_cancels_active_gesture_states() {
    for state in [Gesture::Held, Gesture::Tapped, Gesture::Locked] {
        let mut r = rig();
        r.handle.send(Event::Down(0.0));
        r.controller.drain();
        if matches!(state, Gesture::Tapped | Gesture::Locked) {
            r.handle.send(Event::Up(0.1));
            r.controller.drain();
        }
        if state == Gesture::Locked {
            r.handle.send(Event::Down(0.2));
            r.controller.drain();
        }
        assert_eq!(r.controller.gesture(), state);
        r.handle.send(Event::Cancel(0.25));
        r.controller.drain();
        assert!(!r.recording());
        let expected = if state == Gesture::Tapped {
            Gesture::Idle
        } else {
            Gesture::AwaitUp
        };
        assert_eq!(r.controller.gesture(), expected, "{state:?}");
        assert_eq!(r.jobs(), 0);
    }
}

#[test]
fn escape_is_ignored_while_idle() {
    let mut r = rig();
    r.handle.send(Event::Cancel(0.0));
    r.controller.drain();
    assert!(r.sounds.lock().unwrap().is_empty());
}

#[test]
fn max_duration_warns_once_then_stops_and_queues() {
    let mut r = rig();
    r.handle.send(Event::Start(0.0));
    r.controller.drain();
    assert!(r.recording());
    r.set_now(1140.0);
    r.controller.check_timers(1140.0);
    r.controller.check_timers(1140.0);
    assert_eq!(
        r.sounds
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "warn")
            .count(),
        1
    );
    r.set_now(1200.0);
    r.controller.check_timers(1200.0);
    assert!(!r.recording());
    assert_eq!(r.jobs(), 1);
}

#[test]
fn idle_consumer_has_no_deadline() {
    let mut r = rig();
    assert!(r.controller.timer_timeout().is_none());
    r.handle.send(Event::Start(0.0));
    r.controller.drain();
    // Recording: only the max-duration warning deadline (1140 s away).
    let t = r.controller.timer_timeout().unwrap();
    assert!(t.as_secs_f64() > 1000.0);
}

#[test]
fn toggle_intent_resolves_under_rapid_double_press() {
    let mut r = rig();
    r.handle.note_toggle();
    r.set_now(1.0);
    r.handle.note_toggle();
    r.controller.drain();
    assert_eq!(r.start_calls(), 1);
    assert_eq!(r.jobs(), 1);
}

#[test]
fn command_mode_creates_command_job_with_selection() {
    let mut r = rig();
    {
        let mut out = r.output.0.lock().unwrap();
        out.selection = "selected".into();
        out.clipboard_before = Some("before".into());
    }
    r.handle.send(Event::CommandDown(0.0));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Command);
    r.handle.send(Event::CommandUp(0.1));
    r.controller.drain();
    let jobs = r.jobs.lock().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, JobKind::Command);
    assert_eq!(jobs[0].selection, "selected");
    assert_eq!(jobs[0].clipboard_before.as_deref(), Some("before"));
}

#[test]
fn active_command_cancel_discards_and_restores_clipboard() {
    let mut r = rig();
    r.output.0.lock().unwrap().selection = "sel".into();
    r.handle.send(Event::CommandDown(0.0));
    r.handle.send(Event::CommandCancel(0.05));
    r.handle.send(Event::CommandUp(0.1));
    r.controller.drain();
    assert_eq!(r.jobs(), 0);
    assert!(!r.recording());
    assert_eq!(r.output.0.lock().unwrap().restored.len(), 1);
}

#[test]
fn transform_needs_a_selection() {
    let mut r = rig();
    r.handle.note_transform("polish");
    r.controller.drain();
    assert_eq!(r.jobs(), 0);
    assert_eq!(r.sounds.lock().unwrap().last().unwrap(), "error");
    r.output.0.lock().unwrap().selection = "text".into();
    r.handle.note_transform("polish");
    r.controller.drain();
    let jobs = r.jobs.lock().unwrap();
    assert_eq!(jobs[0].kind, JobKind::Transform);
    assert_eq!(jobs[0].preset.as_deref(), Some("polish"));
}

#[test]
fn job_result_pastes_and_failed_result_restores_selection() {
    let mut r = rig();
    r.handle.note_job_result(
        Some("hi".into()),
        Some("before".into()),
        false,
        Some(String::new()),
    );
    r.handle
        .note_job_result(None, Some("before".into()), true, Some("sel".into()));
    r.controller.drain();
    let out = r.output.0.lock().unwrap();
    assert_eq!(
        out.pasted,
        vec![("hi".into(), "auto".into(), Some("before".into()))]
    );
    assert_eq!(out.restored, vec![("sel".into(), Some("before".into()))]);
    drop(out);
    // Paste last now uses the newest result.
    r.handle.note_paste_last();
    r.controller.drain();
    assert_eq!(r.output.0.lock().unwrap().pasted.last().unwrap().0, "hi");
}

#[test]
fn paste_last_uses_saved_text_on_startup() {
    let mut r = rig();
    r.handle.note_paste_last();
    r.controller.drain();
    assert_eq!(r.output.0.lock().unwrap().pasted[0].0, "saved text");
}

#[test]
fn mic_error_returns_to_idle() {
    let mut r = rig();
    r.audio.0.lock().unwrap().fail_start = true;
    r.handle.send(Event::Down(0.0));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::AwaitUp);
    r.handle.send(Event::Up(1.0));
    r.controller.drain();
    assert_eq!(r.controller.gesture(), Gesture::Idle);
}

// ---------------------------------------------------------- dictation switch

fn disabled() -> lf_core::config::Config {
    let mut c = config();
    c.set_field("dictation_enabled", serde_json::json!(false));
    c
}

#[test]
fn disabled_ignores_every_gesture_and_never_opens_the_mic() {
    let mut r = rig_with(disabled());
    for event in [
        Event::Down(0.0),
        Event::Up(1.0),
        Event::Start(2.0),
        Event::Stop(3.0),
        Event::CommandDown(4.0),
        Event::CommandUp(5.0),
    ] {
        r.handle.send(event);
        r.controller.drain();
    }
    r.handle.note_toggle();
    r.controller.drain();
    assert_eq!(r.start_calls(), 0);
    assert_eq!(r.jobs(), 0);
    assert!(r.handle.is_idle());
}

#[test]
fn paste_last_still_works_while_disabled() {
    let mut r = rig_with(disabled());
    r.handle.note_paste_last();
    r.controller.drain();
    assert_eq!(r.output.0.lock().unwrap().pasted.len(), 1);
}

#[test]
fn toggle_works_while_disabled_and_persists() {
    let mut r = rig_with(disabled());
    r.handle.note_dictation_toggle();
    r.controller.drain();
    assert!(lf_runtime::settings::dictation_enabled(&r.handle.config()));
    assert_eq!(*r.toggles.lock().unwrap(), vec![true]);
    r.handle.send(Event::Down(0.0));
    r.controller.drain();
    assert!(r.recording());
    r.handle.send(Event::Up(1.0));
    r.controller.drain();
    r.handle.note_dictation_toggle();
    r.controller.drain();
    assert!(!lf_runtime::settings::dictation_enabled(&r.handle.config()));
    assert_eq!(*r.toggles.lock().unwrap(), vec![true, false]);
}

#[test]
fn toggle_save_error_keeps_state() {
    let mut r = rig();
    *r.stored_enabled.lock().unwrap() = Err("disk unavailable".into());
    r.handle.note_dictation_toggle();
    r.controller.drain();
    assert!(lf_runtime::settings::dictation_enabled(&r.handle.config()));
    assert_eq!(r.start_calls(), 0);
}

#[test]
fn disabling_discards_active_recording() {
    let mut r = rig();
    r.handle.send(Event::Down(0.0));
    r.controller.drain();
    assert!(r.recording());
    r.handle.update_config(Arc::new(disabled()));
    r.controller.drain();
    assert!(!r.recording());
    r.handle.send(Event::Up(1.0));
    r.controller.drain();
    let audio = r.audio.0.lock().unwrap();
    assert_eq!((audio.start_calls, audio.stop_calls), (1, 1));
    drop(audio);
    assert_eq!(r.jobs(), 0);
    assert!(r.handle.is_idle());
    assert_eq!(r.controller.gesture(), Gesture::Idle);
}

#[test]
fn threaded_consumer_blocks_until_events() {
    let r = rig();
    let handle = r.handle.clone();
    let jobs = r.jobs.clone();
    let thread = r.controller.spawn();
    handle.send(Event::Down(0.0));
    handle.send(Event::Up(1.0));
    handle.shutdown();
    thread.join().unwrap();
    assert_eq!(jobs.lock().unwrap().len(), 1);
}
