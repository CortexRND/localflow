//! Menu bar model and meeting-status polling (`localflow/menubar.py`,
//! `localflow/menubar_poll.py`), kept free of AppKit so it can be tested.
//!
//! In one process the dictation state is pushed to the menu directly by the
//! controller (no status.json polling). Only the meeting server is polled,
//! on the adaptive 3–30 s schedule, from a thread that sleeps in between.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::settings::dictation_label;
use crate::status::title_prefix;

pub const ACTIVE_INTERVAL: f64 = 3.0;
pub const IDLE_INTERVAL: f64 = 5.0;
pub const DOWN_MIN_INTERVAL: f64 = 5.0;
pub const DOWN_MAX_INTERVAL: f64 = 30.0;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub const IDLE_ICON: &str = "🎙";
pub const DETECTED_ICON: &str = "🎙❗";
pub const LIVE_ICON: &str = "🔴";
pub const DOWN_ICON: &str = "🎙⚫";

#[derive(Clone, Debug, PartialEq)]
pub enum PollResult {
    Ok(Value),
    Down,
}

/// `MeetingStatusPoller.tick` scheduling, driven by an explicit clock.
pub struct PollSchedule {
    next_due: f64,
    down_interval: f64,
}

impl Default for PollSchedule {
    fn default() -> Self {
        PollSchedule {
            next_due: 0.0,
            down_interval: DOWN_MIN_INTERVAL,
        }
    }
}

impl PollSchedule {
    /// Ask for a request now (after a user action).
    pub fn poke(&mut self) {
        self.next_due = 0.0;
    }

    pub fn due(&self, now: f64) -> bool {
        now >= self.next_due
    }

    pub fn next_due(&self) -> f64 {
        self.next_due
    }

    /// A request started: guard against a hung one.
    pub fn started(&mut self, now: f64) {
        self.next_due = now + DOWN_MAX_INTERVAL;
    }

    /// A request finished: schedule the next.
    pub fn finished(&mut self, result: &PollResult, now: f64) {
        let interval = match result {
            PollResult::Down => {
                let interval = self.down_interval;
                self.down_interval = (self.down_interval * 2.0).min(DOWN_MAX_INTERVAL);
                interval
            }
            PollResult::Ok(status) => {
                self.down_interval = DOWN_MIN_INTERVAL;
                if status.get("session").is_some_and(|s| !s.is_null()) {
                    ACTIVE_INTERVAL
                } else {
                    IDLE_INTERVAL
                }
            }
        };
        if self.next_due != 0.0 {
            self.next_due = now + interval;
        }
    }
}

/// Runs `fetch` on its own thread on the adaptive schedule; results go to
/// `on_result`. Sleeps on a condvar between requests (poke wakes it).
pub struct MeetingStatusPoller {
    shared: Arc<(Mutex<PollerState>, Condvar)>,
}

struct PollerState {
    schedule: PollSchedule,
    stop: bool,
}

impl MeetingStatusPoller {
    pub fn start(
        fetch: Arc<dyn Fn() -> Result<Value, String> + Send + Sync>,
        on_result: Arc<dyn Fn(PollResult) + Send + Sync>,
    ) -> Self {
        let shared = Arc::new((
            Mutex::new(PollerState {
                schedule: PollSchedule::default(),
                stop: false,
            }),
            Condvar::new(),
        ));
        let worker = shared.clone();
        let epoch = Instant::now();
        std::thread::Builder::new()
            .name("menubar-status".into())
            .spawn(move || loop {
                let (lock, wake) = &*worker;
                {
                    let mut st = lock.lock().unwrap_or_else(|p| p.into_inner());
                    loop {
                        if st.stop {
                            return;
                        }
                        let now = epoch.elapsed().as_secs_f64();
                        if st.schedule.due(now) {
                            st.schedule.started(now);
                            break;
                        }
                        let wait = Duration::from_secs_f64(st.schedule.next_due() - now);
                        st = wake
                            .wait_timeout(st, wait)
                            .unwrap_or_else(|p| p.into_inner())
                            .0;
                    }
                }
                let result = match fetch() {
                    Ok(status) if status.is_object() => PollResult::Ok(status),
                    Ok(_) => PollResult::Ok(Value::Object(Default::default())),
                    Err(_) => PollResult::Down,
                };
                lock.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .schedule
                    .finished(&result, epoch.elapsed().as_secs_f64());
                on_result(result);
            })
            .expect("spawn menubar poller");
        MeetingStatusPoller { shared }
    }

    pub fn poke(&self) {
        let (lock, wake) = &*self.shared;
        lock.lock()
            .unwrap_or_else(|p| p.into_inner())
            .schedule
            .poke();
        wake.notify_all();
    }
}

impl Drop for MeetingStatusPoller {
    fn drop(&mut self) {
        let (lock, wake) = &*self.shared;
        lock.lock().unwrap_or_else(|p| p.into_inner()).stop = true;
        wake.notify_all();
    }
}

/// Everything the menu shows, recomputed from pushed state.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuModel {
    pub app_state: String,
    pub dictation_enabled: bool,
    pub base_icon: &'static str,
    pub status_line: String,
    pub meeting_toggle: &'static str,
    pub session_active: bool,
    pub detected: bool,
    pub input_warning: Option<String>,
}

impl Default for MenuModel {
    fn default() -> Self {
        MenuModel {
            app_state: "idle".into(),
            dictation_enabled: true,
            base_icon: IDLE_ICON,
            status_line: "server: …".into(),
            meeting_toggle: "Start meeting transcription",
            session_active: false,
            detected: false,
            input_warning: None,
        }
    }
}

impl MenuModel {
    pub fn title(&self) -> String {
        let suffix = if self.dictation_enabled { "" } else { " (off)" };
        format!(
            "{}{}{}",
            title_prefix(&self.app_state),
            self.base_icon,
            suffix
        )
    }

    pub fn dictation_item(&self) -> &'static str {
        dictation_label(self.dictation_enabled)
    }

    /// `_apply_status`: what the server poll says about meetings.
    pub fn apply_status(&mut self, result: &PollResult) {
        let status = match result {
            PollResult::Down => {
                self.base_icon = DOWN_ICON;
                self.status_line = "server: down (lf serve)".into();
                self.session_active = false;
                return;
            }
            PollResult::Ok(status) => status,
        };
        let truthy = |v: Option<&Value>| match v {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        };
        let session = status.get("session").filter(|s| truthy(Some(s)));
        self.session_active = session.is_some();
        self.detected = truthy(status.get("detected"));
        if let Some(session) = session {
            let seconds = session
                .get("seconds")
                .and_then(Value::as_f64)
                .unwrap_or(0.0) as i64;
            let title = session
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let segments = session
                .get("segment_count")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            self.base_icon = LIVE_ICON;
            self.status_line = format!(
                "recording: {title} — {}m{:02}s, {segments} segments",
                seconds / 60,
                seconds % 60
            );
            self.meeting_toggle = "Stop & save notes";
            return;
        }
        self.meeting_toggle = "Start meeting transcription";
        if self.detected {
            let platform = status
                .get("platform")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("?");
            self.base_icon = DETECTED_ICON;
            self.status_line = format!("meeting detected ({platform})");
        } else {
            self.base_icon = IDLE_ICON;
            let busy = if truthy(status.get("mic_busy")) {
                "mic in use"
            } else {
                "idle"
            };
            self.status_line = format!("watching · {busy}");
        }
    }
}

/// Menu actions the platform UI invokes.
#[derive(Clone)]
pub struct MenuActions {
    pub toggle_dictation: Arc<dyn Fn() + Send + Sync>,
    pub paste_last: Arc<dyn Fn() + Send + Sync>,
    pub quit: Arc<dyn Fn() + Send + Sync>,
}

/// A platform menu bar. `update` may be called from any thread.
pub trait MenuBar: Send + Sync {
    fn update(&self, model: &MenuModel);
}

/// Shared model + the UI it renders into.
#[derive(Clone)]
pub struct MenuState {
    model: Arc<Mutex<MenuModel>>,
    ui: Option<Arc<dyn MenuBar>>,
}

impl MenuState {
    pub fn new(ui: Option<Arc<dyn MenuBar>>, dictation_enabled: bool) -> Self {
        let model = MenuModel {
            dictation_enabled,
            ..Default::default()
        };
        MenuState {
            model: Arc::new(Mutex::new(model)),
            ui,
        }
    }

    pub fn snapshot(&self) -> MenuModel {
        self.model.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn change(&self, f: impl FnOnce(&mut MenuModel)) {
        let model = {
            let mut m = self.model.lock().unwrap_or_else(|p| p.into_inner());
            let before = m.clone();
            f(&mut m);
            if *m == before {
                return;
            }
            m.clone()
        };
        if let Some(ui) = &self.ui {
            ui.update(&model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schedule_backs_off_while_down_and_resets_when_up() {
        let mut s = PollSchedule::default();
        assert!(s.due(0.0));
        s.started(0.0);
        assert!(!s.due(1.0));
        s.finished(&PollResult::Down, 1.0);
        assert_eq!(s.next_due(), 1.0 + DOWN_MIN_INTERVAL);
        s.started(6.0);
        s.finished(&PollResult::Down, 6.0);
        assert_eq!(s.next_due(), 6.0 + 10.0);
        for _ in 0..5 {
            s.started(20.0);
            s.finished(&PollResult::Down, 20.0);
        }
        assert_eq!(s.next_due(), 20.0 + DOWN_MAX_INTERVAL);
        s.started(50.0);
        s.finished(&PollResult::Ok(json!({"session": {"title": "x"}})), 50.0);
        assert_eq!(s.next_due(), 50.0 + ACTIVE_INTERVAL);
        s.started(60.0);
        s.finished(&PollResult::Ok(json!({})), 60.0);
        assert_eq!(s.next_due(), 60.0 + IDLE_INTERVAL);
        // A poke during a request means "again right after it finishes".
        s.started(70.0);
        s.poke();
        s.finished(&PollResult::Ok(json!({})), 70.5);
        assert!(s.due(70.5));
    }

    #[test]
    fn model_titles() {
        let mut m = MenuModel::default();
        assert_eq!(m.title(), "🎙");
        assert_eq!(m.dictation_item(), "Dictation: On");
        m.dictation_enabled = false;
        m.app_state = "recording".into();
        assert_eq!(m.title(), "● 🎙 (off)");
        assert_eq!(m.dictation_item(), "Dictation: Off");
        m.apply_status(&PollResult::Down);
        assert_eq!(m.status_line, "server: down (lf serve)");
        m.apply_status(&PollResult::Ok(
            json!({"session": {"title": "Standup", "seconds": 125, "segment_count": 4}}),
        ));
        assert_eq!(m.status_line, "recording: Standup — 2m05s, 4 segments");
        assert_eq!(m.meeting_toggle, "Stop & save notes");
        m.apply_status(&PollResult::Ok(
            json!({"detected": true, "platform": "Zoom"}),
        ));
        assert_eq!(m.status_line, "meeting detected (Zoom)");
        assert_eq!(m.base_icon, DETECTED_ICON);
    }

    #[test]
    fn state_pushes_only_on_change() {
        struct Count(Mutex<usize>);
        impl MenuBar for Count {
            fn update(&self, _m: &MenuModel) {
                *self.0.lock().unwrap() += 1;
            }
        }
        let ui = Arc::new(Count(Mutex::new(0)));
        let state = MenuState::new(Some(ui.clone()), true);
        state.change(|m| m.app_state = "recording".into());
        state.change(|m| m.app_state = "recording".into());
        state.change(|m| m.dictation_enabled = false);
        assert_eq!(*ui.0.lock().unwrap(), 2);
        assert!(!state.snapshot().dictation_enabled);
    }
}
