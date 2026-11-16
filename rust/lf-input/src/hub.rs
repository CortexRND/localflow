//! `KeyboardHub`: hold key, toggle chord, global chords and command mode
//! routed through one listener (port of `localflow/hotkey.py`).
//!
//! The platform listener feeds `press`/`release`; callbacks run after the
//! internal lock is released. Rebinding is deferred while a key is down or the
//! app is busy, so a gesture started with the old shortcut ends with it too.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use crate::keys::{is_hold_key, Chord, Key};

pub type Callback = Arc<dyn Fn() + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// A press whose release was never reported must not block a pending
/// reconfiguration forever once nothing else is held.
pub const STALE_PRESS_SECONDS: f64 = 10.0;
/// Command mode only starts once the key has been held alone this long, so
/// a chord that includes the command key doesn't flash command mode.
pub const COMMAND_DELAY: Duration = Duration::from_millis(120);

/// The controller-side callbacks (`on_down`, `on_up`, ...).
#[derive(Clone)]
pub struct HubActions {
    pub on_down: Callback,
    pub on_up: Callback,
    pub on_toggle: Callback,
    pub on_cancel: Callback,
    pub on_command_down: Callback,
    pub on_command_up: Callback,
    pub on_command_cancel: Callback,
    pub can_reconfigure: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl HubActions {
    pub fn noop() -> Self {
        let nothing: Callback = Arc::new(|| {});
        HubActions {
            on_down: nothing.clone(),
            on_up: nothing.clone(),
            on_toggle: nothing.clone(),
            on_cancel: nothing.clone(),
            on_command_down: nothing.clone(),
            on_command_up: nothing.clone(),
            on_command_cancel: nothing,
            can_reconfigure: Arc::new(|| true),
        }
    }
}

/// One requested set of shortcuts (`hold_key`, `toggle_combo`, combos, command key).
#[derive(Clone, Default)]
pub struct Shortcuts {
    pub hold_key: String,
    pub toggle_combo: String,
    pub combos: Vec<(String, Callback)>,
    pub command_key: String,
}

/// Why a shortcut was disabled while building bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingWarning {
    Duplicate {
        shortcut: String,
        owner: String,
        disabled: String,
    },
    Invalid {
        shortcut: String,
    },
}

impl std::fmt::Display for BindingWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingWarning::Duplicate {
                shortcut,
                owner,
                disabled,
            } => write!(
                f,
                "shortcut {shortcut:?} is already assigned to {owner}; disabling {disabled}"
            ),
            BindingWarning::Invalid { shortcut } => {
                write!(f, "invalid shortcut {shortcut:?}; disabling shortcut")
            }
        }
    }
}

struct Combo {
    chord: Chord,
    state: BTreeSet<Key>,
    callback: Callback,
}

#[derive(Default)]
struct Bindings {
    hold_key: Option<Key>,
    toggle: Option<Combo>,
    combos: Vec<Combo>,
    command_key: Option<Key>,
}

/// The identity a shortcut reserves: single keys keep their side, chords
/// compare as canonical key sets. A chord of one key equals that key alone.
#[derive(PartialEq, Eq, Clone, Debug)]
enum Reserved {
    Single(Key),
    Chord(Chord),
}

fn build(shortcuts: &Shortcuts, on_toggle: &Callback) -> (Bindings, Vec<BindingWarning>) {
    let mut seen: Vec<(Reserved, &'static str)> = Vec::new();
    let mut warnings = Vec::new();
    let mut reserve = |name: &'static str, shortcut: &str, reserved: Option<Reserved>| -> bool {
        if shortcut.is_empty() {
            return false;
        }
        let Some(reserved) = reserved else {
            warnings.push(BindingWarning::Invalid {
                shortcut: shortcut.to_string(),
            });
            return false;
        };
        let clash = seen.iter().find(|(r, _)| match (r, &reserved) {
            (Reserved::Single(a), Reserved::Single(b)) => a == b,
            (Reserved::Chord(a), Reserved::Chord(b)) => a == b,
            (Reserved::Single(a), Reserved::Chord(b))
            | (Reserved::Chord(b), Reserved::Single(a)) => {
                b.keys().len() == 1 && b.contains(&a.canonical())
            }
        });
        if let Some((_, owner)) = clash {
            warnings.push(BindingWarning::Duplicate {
                shortcut: shortcut.to_string(),
                owner: owner.to_string(),
                disabled: name.to_string(),
            });
            return false;
        }
        seen.push((reserved, name));
        true
    };
    let single = |s: &str| Key::from_name(s).map(Reserved::Single);
    let chord = |s: &str| Chord::parse(s).ok().map(Reserved::Chord);

    let mut bindings = Bindings::default();
    if reserve(
        "dictation",
        &shortcuts.hold_key,
        single(&shortcuts.hold_key),
    ) {
        bindings.hold_key = Key::from_name(&shortcuts.hold_key);
    }
    if reserve(
        "dictation",
        &shortcuts.toggle_combo,
        chord(&shortcuts.toggle_combo),
    ) {
        bindings.toggle = Chord::parse(&shortcuts.toggle_combo).ok().map(|c| Combo {
            chord: c,
            state: BTreeSet::new(),
            callback: on_toggle.clone(),
        });
    }
    for (shortcut, callback) in &shortcuts.combos {
        // A bare key name ("f18") bound as a combo is a one-key chord.
        let parsed = if is_hold_key(shortcut) {
            Key::from_name(shortcut).and_then(|k| Chord::parse(&k.to_string()).ok())
        } else {
            Chord::parse(shortcut).ok()
        };
        if reserve("shortcut", shortcut, parsed.clone().map(Reserved::Chord)) {
            if let Some(chord) = parsed {
                bindings.combos.push(Combo {
                    chord,
                    state: BTreeSet::new(),
                    callback: callback.clone(),
                });
            }
        }
    }
    if reserve(
        "command mode",
        &shortcuts.command_key,
        single(&shortcuts.command_key),
    ) {
        bindings.command_key = Key::from_name(&shortcuts.command_key);
    }
    (bindings, warnings)
}

struct State {
    bindings: Bindings,
    pending: Option<Bindings>,
    pending_since: Option<f64>,
    pressed: BTreeSet<Key>,
    pressed_raw: Vec<Key>,
    held_key_down: bool,
    command_key_down: bool,
    command_timer: Option<u64>,
    timer_generation: u64,
    last_event_at: Option<f64>,
}

struct TimerSlot {
    deadline: Option<(Instant, u64)>,
    stop: bool,
}

struct Inner {
    state: Mutex<State>,
    /// Callbacks in decision order: queued under `state`, run under `delivery`,
    /// so a release can never be delivered before the press that preceded it.
    queue: Mutex<VecDeque<Callback>>,
    delivery: Mutex<()>,
    actions: HubActions,
    clock: Clock,
    command_delay: Duration,
    saw_any_event: AtomicBool,
    timer: Arc<TimerShared>,
}

struct TimerShared {
    slot: Mutex<TimerSlot>,
    wake: Condvar,
}

/// Handle to the hub; cheap to clone, shared with the platform listener.
#[derive(Clone)]
pub struct KeyboardHub {
    inner: Arc<Inner>,
}

impl KeyboardHub {
    pub fn new(shortcuts: &Shortcuts, actions: HubActions) -> (KeyboardHub, Vec<BindingWarning>) {
        let clock: Clock = {
            let start = Instant::now();
            Arc::new(move || start.elapsed().as_secs_f64())
        };
        Self::with_options(shortcuts, actions, clock, COMMAND_DELAY)
    }

    pub fn with_options(
        shortcuts: &Shortcuts,
        actions: HubActions,
        clock: Clock,
        command_delay: Duration,
    ) -> (KeyboardHub, Vec<BindingWarning>) {
        let (bindings, warnings) = build(shortcuts, &actions.on_toggle);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                bindings,
                pending: None,
                pending_since: None,
                pressed: BTreeSet::new(),
                pressed_raw: Vec::new(),
                held_key_down: false,
                command_key_down: false,
                command_timer: None,
                timer_generation: 0,
                last_event_at: None,
            }),
            queue: Mutex::new(VecDeque::new()),
            delivery: Mutex::new(()),
            actions,
            clock,
            command_delay,
            saw_any_event: AtomicBool::new(false),
            timer: Arc::new(TimerShared {
                slot: Mutex::new(TimerSlot {
                    deadline: None,
                    stop: false,
                }),
                wake: Condvar::new(),
            }),
        });
        spawn_timer(Arc::downgrade(&inner), inner.timer.clone());
        for warning in &warnings {
            log::warn!("{warning}");
        }
        (KeyboardHub { inner }, warnings)
    }

    /// True once any key event arrived (the macOS tap is alive).
    pub fn saw_any_event(&self) -> bool {
        self.inner.saw_any_event.load(Ordering::SeqCst)
    }

    /// Replace every binding; true when applied now, false when deferred.
    pub fn reconfigure(&self, shortcuts: &Shortcuts) -> (bool, Vec<BindingWarning>) {
        let (bindings, warnings) = build(shortcuts, &self.inner.actions.on_toggle);
        for warning in &warnings {
            log::warn!("{warning}");
        }
        {
            let mut st = self.lock();
            st.pending = Some(bindings);
            if st.pending_since.is_none() {
                st.pending_since = Some((self.inner.clock)());
            }
        }
        (self.apply_pending(), warnings)
    }

    /// Install a pending binding set if it is safe now.
    pub fn apply_pending(&self) -> bool {
        let mut st = self.lock();
        self.apply_pending_locked(&mut st)
    }

    pub fn reconfigure_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    pub fn press(&self, key: Key) {
        {
            let mut st = self.lock();
            st.last_event_at = Some((self.inner.clock)());
            let fired = self.press_locked(&mut st, key);
            self.enqueue(fired);
        }
        self.deliver();
    }

    pub fn release(&self, key: Key) {
        {
            let mut st = self.lock();
            st.last_event_at = Some((self.inner.clock)());
            let fired = self.release_locked(&mut st, &key);
            if st.pending.is_some() && st.pressed_raw.is_empty() {
                self.apply_pending_locked(&mut st);
            }
            self.enqueue(fired);
        }
        self.deliver();
    }

    /// Fire the command-mode timer now (what the timer thread does on expiry).
    pub fn fire_command_timer(&self) {
        self.fire_command_timer_with_gap(|| {});
    }

    /// `fire_command_timer`, running `gap` after the decision but before its
    /// callback is delivered (tests use it to race a key release).
    #[doc(hidden)]
    pub fn fire_command_timer_with_gap(&self, gap: impl FnOnce()) {
        let generation = self.lock().command_timer;
        if let Some(generation) = generation {
            self.command_timer_fired(generation, gap);
        }
    }

    /// Call with the state lock held, so queue order is decision order.
    fn enqueue(&self, fired: Vec<Callback>) {
        if !fired.is_empty() {
            self.inner
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(fired);
        }
    }

    /// Run queued callbacks one at a time, in order. A thread that finds
    /// delivery busy (another thread, or a callback re-entering the hub)
    /// leaves its callbacks to the deliverer, which re-checks the queue after
    /// letting go, so nothing is stranded and nothing deadlocks.
    fn deliver(&self) {
        loop {
            let _guard = match self.inner.delivery.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            while let Some(callback) = self.pop() {
                run(callback);
            }
            drop(_guard);
            if self
                .inner
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            {
                return;
            }
        }
    }

    fn pop(&self) -> Option<Callback> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn safe_to_swap(&self, st: &State) -> bool {
        if st.held_key_down || st.command_key_down || st.command_timer.is_some() {
            return false;
        }
        if !(self.inner.actions.can_reconfigure)() {
            return false;
        }
        if st.pressed_raw.is_empty() {
            return true;
        }
        let last = st.last_event_at.or(st.pending_since);
        last.is_some_and(|t| (self.inner.clock)() - t >= STALE_PRESS_SECONDS)
    }

    fn apply_pending_locked(&self, st: &mut State) -> bool {
        if st.pending.is_none() || !self.safe_to_swap(st) {
            return false;
        }
        st.bindings = st.pending.take().unwrap_or_default();
        st.pending_since = None;
        st.pressed.clear();
        st.pressed_raw.clear();
        log::info!("keyboard shortcuts updated");
        true
    }

    fn command_combo_key(st: &State) -> Option<Key> {
        st.bindings.command_key.as_ref().map(Key::canonical)
    }

    fn is_combo_chord(st: &State) -> bool {
        let Some(command) = Self::command_combo_key(st) else {
            return false;
        };
        st.bindings.combos.iter().any(|combo| {
            combo.chord.contains(&command)
                && combo
                    .chord
                    .keys()
                    .iter()
                    .any(|k| *k != command && st.pressed.contains(k))
        })
    }

    fn note_event(&self) {
        if !self.inner.saw_any_event.swap(true, Ordering::SeqCst) {
            log::info!("first keyboard event received — Input Monitoring OK");
        }
    }

    fn press_locked(&self, st: &mut State, key: Key) -> Vec<Callback> {
        self.note_event();
        let mut fired = Vec::new();
        let canonical = key.canonical();
        if st.pressed_raw.contains(&key) {
            return fired;
        }
        st.pressed_raw.push(key.clone());
        st.pressed.insert(canonical.clone());
        if st.command_timer.is_some() && Self::is_combo_chord(st) {
            self.cancel_timer(st);
        }
        let actions = &self.inner.actions;
        if canonical == Key::ESC {
            fired.push(actions.on_cancel.clone());
        }
        if st.bindings.hold_key.as_ref() == Some(&key) {
            st.held_key_down = true;
            fired.push(actions.on_down.clone());
        }
        if st.bindings.command_key.as_ref() == Some(&key) && !Self::is_combo_chord(st) {
            self.arm_timer(st);
        }
        let command = Self::command_combo_key(st);
        let mut cancel_command = false;
        if let Some(toggle) = st.bindings.toggle.as_mut() {
            if combo_press(toggle, &canonical) {
                fired.push(toggle.callback.clone());
            }
        }
        for combo in st.bindings.combos.iter_mut() {
            if combo_press(combo, &canonical) {
                if st.command_key_down && command.as_ref().is_some_and(|c| combo.chord.contains(c))
                {
                    cancel_command = true;
                    fired.push(actions.on_command_cancel.clone());
                }
                fired.push(combo.callback.clone());
            }
        }
        if cancel_command {
            st.command_key_down = false;
        }
        fired
    }

    fn release_locked(&self, st: &mut State, key: &Key) -> Vec<Callback> {
        self.note_event();
        let mut fired = Vec::new();
        let canonical = key.canonical();
        st.pressed_raw.retain(|k| k != key);
        if !st.pressed_raw.iter().any(|k| k.canonical() == canonical) {
            st.pressed.remove(&canonical);
            if let Some(toggle) = st.bindings.toggle.as_mut() {
                toggle.state.remove(&canonical);
            }
            for combo in st.bindings.combos.iter_mut() {
                combo.state.remove(&canonical);
            }
        }
        let actions = &self.inner.actions;
        if st.bindings.hold_key.as_ref() == Some(key) && st.held_key_down {
            st.held_key_down = false;
            fired.push(actions.on_up.clone());
        }
        if st.bindings.command_key.as_ref() == Some(key) {
            if st.command_timer.is_some() {
                self.cancel_timer(st);
            } else if st.command_key_down {
                st.command_key_down = false;
                fired.push(actions.on_command_up.clone());
            }
        }
        fired
    }

    fn arm_timer(&self, st: &mut State) {
        st.timer_generation += 1;
        let generation = st.timer_generation;
        st.command_timer = Some(generation);
        let mut slot = self
            .inner
            .timer
            .slot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        slot.deadline = Some((Instant::now() + self.inner.command_delay, generation));
        self.inner.timer.wake.notify_all();
    }

    fn cancel_timer(&self, st: &mut State) {
        st.command_timer = None;
        let mut slot = self
            .inner
            .timer
            .slot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        slot.deadline = None;
        self.inner.timer.wake.notify_all();
    }

    fn command_timer_fired(&self, generation: u64, gap: impl FnOnce()) {
        {
            let mut st = self.lock();
            if st.command_timer != Some(generation) {
                return;
            }
            st.command_timer = None;
            let still_down = st
                .bindings
                .command_key
                .as_ref()
                .is_some_and(|k| st.pressed_raw.contains(k));
            if st.command_key_down || !still_down || Self::is_combo_chord(&st) {
                return;
            }
            st.command_key_down = true;
            self.enqueue(vec![self.inner.actions.on_command_down.clone()]);
        }
        gap();
        self.deliver();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let mut slot = self.timer.slot.lock().unwrap_or_else(|p| p.into_inner());
        slot.stop = true;
        self.timer.wake.notify_all();
    }
}

/// pynput `HotKey.press`: true when this press completes the chord.
fn combo_press(combo: &mut Combo, key: &Key) -> bool {
    if combo.chord.contains(key) && combo.state.insert(key.clone()) {
        return combo.state.len() == combo.chord.keys().len();
    }
    false
}

fn run(callback: Callback) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback())).is_err() {
        log::error!("keyboard shortcut callback failed");
    }
}

/// One thread per hub that sleeps on a condvar until the command timer is
/// armed: no polling while idle. It only holds a weak ref to the hub.
fn spawn_timer(weak: Weak<Inner>, timer: Arc<TimerShared>) {
    std::thread::Builder::new()
        .name("lf-hotkey-timer".into())
        .spawn(move || loop {
            let due = {
                let mut slot = timer.slot.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if slot.stop {
                        return;
                    }
                    match slot.deadline {
                        None => {
                            slot = timer.wake.wait(slot).unwrap_or_else(|p| p.into_inner());
                        }
                        Some((at, generation)) => {
                            let now = Instant::now();
                            if now >= at {
                                slot.deadline = None;
                                break generation;
                            }
                            slot = timer
                                .wake
                                .wait_timeout(slot, at - now)
                                .unwrap_or_else(|p| p.into_inner())
                                .0;
                        }
                    }
                }
            };
            match weak.upgrade() {
                Some(inner) => KeyboardHub { inner }.command_timer_fired(due, || {}),
                None => return,
            }
        })
        .expect("spawn hotkey timer thread");
}
