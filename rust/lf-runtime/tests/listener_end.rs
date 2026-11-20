//! End to end with fakes only: scripted X11-style key source → the real
//! `lf_input::linux::pump` → the real hub with the runtime's own
//! `hub_actions` → the real controller, then the runtime's own exit
//! handling (`listener_lost`). No X server, keyboard hook or microphone.

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{rig, Rig};
use lf_input::hub::{KeyboardHub, Shortcuts};
use lf_input::linux::{pump, KeySource, RawKey};
use lf_input::InputError;
use lf_runtime::controller::JobKind;

const F19: u32 = 10;
const ALT_R: u32 = 11;
const CTRL: u32 = 12;
const ALT_L: u32 = 13;
const KEY_T: u32 = 14;

enum Step {
    Key(RawKey),
    /// The command-mode delay elapses (deterministic, no real timer).
    FireCommandTimer,
    /// Advance the controller clock.
    Now(f64),
    /// The X connection dies.
    Fail,
    /// A deliberate stop (shutdown).
    Stop,
}

struct Script {
    steps: VecDeque<Step>,
    hub: KeyboardHub,
    now: Arc<Mutex<f64>>,
    stop: Arc<AtomicBool>,
}

impl KeySource for Script {
    fn next(&mut self, _wait: Duration) -> Result<Option<RawKey>, String> {
        loop {
            match self.steps.pop_front() {
                Some(Step::Key(key)) => return Ok(Some(key)),
                Some(Step::FireCommandTimer) => self.hub.fire_command_timer(),
                Some(Step::Now(t)) => *self.now.lock().unwrap() = t,
                Some(Step::Fail) => return Err("X connection closed".into()),
                Some(Step::Stop) | None => {
                    self.stop.store(true, Ordering::Release);
                    return Ok(None);
                }
            }
        }
    }

    fn keysyms(&self, code: u32) -> [u32; 2] {
        let sym = match code {
            F19 => 0xffbe + 18,
            ALT_R => 0xffea,
            CTRL => 0xffe3,
            ALT_L => 0xffe9,
            KEY_T => 't' as u32,
            _ => 0,
        };
        [sym, sym]
    }
}

/// Run `steps` through pump → hub → controller, then what the runtime does
/// when the listener exits unasked. Returns the pump result.
fn run(r: &mut Rig, steps: Vec<Step>) -> Result<(), InputError> {
    let shortcuts = Shortcuts {
        hold_key: "f19".into(),
        command_key: "alt_r".into(),
        toggle_combo: "<ctrl>+<alt>+t".into(),
        ..Default::default()
    };
    let (hub, warnings) = KeyboardHub::with_options(
        &shortcuts,
        lf_runtime::app::hub_actions(&r.handle),
        Arc::new(|| 0.0),
        // Only FireCommandTimer enters command mode.
        Duration::from_secs(3600),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let stop = Arc::new(AtomicBool::new(false));
    let mut source = Script {
        steps: steps.into(),
        hub: hub.clone(),
        now: r.now.clone(),
        stop: stop.clone(),
    };
    let result = pump(&mut source, &hub, &stop);
    if result.is_err() {
        lf_runtime::app::listener_lost(&hub, &r.handle);
    }
    r.controller.drain();
    result
}

fn kinds(r: &Rig) -> Vec<JobKind> {
    r.jobs.lock().unwrap().iter().map(|j| j.kind).collect()
}

fn command_down() -> Vec<Step> {
    vec![
        Step::Key(RawKey::Press(ALT_R)),
        Step::FireCommandTimer,
        Step::Now(2.0),
    ]
}

#[test]
fn a_listener_failure_never_submits_an_unfinished_command() {
    let mut r = rig();
    r.output.0.lock().unwrap().selection = "selected".into();
    let mut steps = command_down();
    steps.push(Step::Fail);
    let result = run(&mut r, steps);
    assert_eq!(
        result,
        Err(InputError::Backend("X connection closed".into()))
    );
    assert!(kinds(&r).is_empty(), "no command job: {:?}", kinds(&r));
    assert!(!r.recording(), "microphone released");
    let out = r.output.0.lock().unwrap();
    assert_eq!(out.restored.len(), 1, "selection clipboard restored");
    assert!(out.pasted.is_empty());
}

#[test]
fn a_deliberate_stop_also_cancels_an_unfinished_command() {
    let mut r = rig();
    let mut steps = command_down();
    steps.push(Step::Stop);
    assert_eq!(run(&mut r, steps), Ok(()));
    assert!(kinds(&r).is_empty());
    assert!(!r.recording());
}

#[test]
fn a_real_key_up_still_submits_the_command_and_a_hold_still_completes() {
    // Physical release before the failure: the command is submitted.
    let mut r = rig();
    let mut steps = command_down();
    steps.extend([Step::Key(RawKey::Release(ALT_R)), Step::Fail]);
    assert!(run(&mut r, steps).is_err());
    assert_eq!(kinds(&r), vec![JobKind::Command]);

    // A hold that the failure ends is transcribed normally.
    let mut r = rig();
    let steps = vec![Step::Key(RawKey::Press(F19)), Step::Now(1.5), Step::Fail];
    assert!(run(&mut r, steps).is_err());
    assert_eq!(kinds(&r), vec![JobKind::Dictate]);
    assert!(!r.recording());
}

#[test]
fn a_listener_failure_discards_a_hands_free_recording() {
    let mut r = rig();
    let steps = vec![
        Step::Key(RawKey::Press(CTRL)),
        Step::Key(RawKey::Press(ALT_L)),
        Step::Key(RawKey::Press(KEY_T)),
        Step::Key(RawKey::Release(KEY_T)),
        Step::Key(RawKey::Release(ALT_L)),
        Step::Key(RawKey::Release(CTRL)),
        Step::Now(3.0),
        Step::Fail,
    ];
    assert!(run(&mut r, steps).is_err());
    assert_eq!(r.start_calls(), 1, "the toggle did start recording");
    assert!(kinds(&r).is_empty(), "{:?}", kinds(&r));
    assert!(!r.recording());
}
