use anyhow::Result;
use std::{
    process::Command,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub trait MicUsageProbe: Send + Sync {
    fn mic_in_use(&self) -> Result<bool>;
}
pub type ProbeFactory = Arc<dyn Fn() -> Result<Box<dyn MicUsageProbe>> + Send + Sync>;
pub type DetectionCallback = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
pub type NotifyCallback = Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>;
pub type PlatformDetector = Arc<dyn Fn() -> String + Send + Sync>;
pub const PLATFORM_PROCESSES: &[(&str, &str, &str)] = &[
    ("Zoom", "-x", "zoom.us"),
    ("Teams", "-f", "Microsoft Teams"),
    ("FaceTime", "-x", "FaceTime"),
    ("Webex", "-f", "Webex"),
    ("Slack", "-x", "Slack"),
    ("Discord", "-x", "Discord"),
];
pub fn detect_platform_with(mut running: impl FnMut(&str, &str) -> bool) -> String {
    PLATFORM_PROCESSES
        .iter()
        .find(|(_, flag, process)| running(flag, process))
        .map(|(name, _, _)| *name)
        .unwrap_or("Other")
        .into()
}
pub fn detect_platform() -> String {
    detect_platform_with(|flag, process| {
        Command::new("pgrep")
            .args([flag, process])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatcherStatus {
    pub detected: bool,
    pub platform: String,
    pub mic_busy: bool,
    pub error: String,
}
#[derive(Default)]
struct State {
    status: WatcherStatus,
    busy_since: Option<Duration>,
    notified: bool,
    paused: bool,
    stopped: bool,
    running: bool,
}
impl State {
    fn observe(&mut self, busy: bool, now: Duration, min_busy: Duration) -> bool {
        self.status.mic_busy = busy;
        if !busy {
            self.busy_since = None;
            self.status.detected = false;
            self.notified = false;
            return false;
        }
        let since = *self.busy_since.get_or_insert(now);
        if now.saturating_sub(since) >= min_busy && !self.status.detected {
            self.status.detected = true;
            return true;
        }
        false
    }
}
#[derive(Default)]
struct Control {
    state: Mutex<State>,
    wake: Condvar,
}
pub struct MeetingWatcher {
    control: Arc<Control>,
    min_busy_seconds: Duration,
    poll_seconds: Duration,
    factory: ProbeFactory,
    on_detect: Option<DetectionCallback>,
    notifier: Option<NotifyCallback>,
    platform_detector: PlatformDetector,
}
impl Default for MeetingWatcher {
    fn default() -> Self {
        Self::new(Duration::from_secs(12), Duration::from_secs(3), None)
            .expect("positive default poll interval")
    }
}
fn default_probe() -> Result<Box<dyn MicUsageProbe>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::macos_audio::CoreAudioMicUsageProbe))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow::anyhow!("CoreAudio not found"))
    }
}
fn call_safely(callback: impl FnOnce() -> Result<()>) {
    if !matches!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)),
        Ok(Ok(()))
    ) {
        log::warn!("meeting watcher callback failed");
    }
}
impl MeetingWatcher {
    pub fn new(
        min_busy_seconds: Duration,
        poll_seconds: Duration,
        on_detect: Option<DetectionCallback>,
    ) -> Result<Self> {
        anyhow::ensure!(!poll_seconds.is_zero(), "poll_seconds must be positive");
        Ok(Self {
            control: Arc::default(),
            min_busy_seconds,
            poll_seconds,
            factory: Arc::new(default_probe),
            on_detect,
            notifier: None,
            platform_detector: Arc::new(detect_platform),
        })
    }
    pub fn with_probe_factory(mut self, factory: ProbeFactory) -> Self {
        self.factory = factory;
        self
    }
    pub fn with_notifier(mut self, notifier: NotifyCallback) -> Self {
        self.notifier = Some(notifier);
        self
    }
    pub fn with_platform_detector(mut self, detector: PlatformDetector) -> Self {
        self.platform_detector = detector;
        self
    }
    pub fn status(&self) -> WatcherStatus {
        self.control
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }
    pub fn start(&self) {
        let mut state = self.control.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.running {
            state.stopped = false;
            return;
        }
        let probe = match (self.factory)() {
            Ok(probe) => probe,
            Err(error) => {
                state.status.error = format!("mic watcher unavailable: {error}");
                return;
            }
        };
        state.status.error.clear();
        state.stopped = false;
        state.running = true;
        let control = self.control.clone();
        let min_busy = self.min_busy_seconds;
        let poll = self.poll_seconds;
        let on_detect = self.on_detect.clone();
        let notifier = self.notifier.clone();
        let detector = self.platform_detector.clone();
        thread::spawn(move || {
            let epoch = Instant::now();
            loop {
                let state = control.state.lock().unwrap_or_else(|e| e.into_inner());
                let (mut state, _) = control
                    .wake
                    .wait_timeout_while(state, poll, |state| !state.stopped)
                    .unwrap_or_else(|e| e.into_inner());
                if state.stopped {
                    state.running = false;
                    return;
                }
                if state.paused {
                    continue;
                }
                drop(state);
                let busy = match probe.mic_in_use() {
                    Ok(busy) => busy,
                    Err(_) => {
                        log::warn!("mic probe failed");
                        continue;
                    }
                };
                let mut state = control.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.stopped || state.paused {
                    continue;
                }
                if !state.observe(busy, epoch.elapsed(), min_busy) {
                    continue;
                }
                state.status.platform = detector();
                let platform = state.status.platform.clone();
                let notify = !state.notified;
                state.notified = true;
                drop(state);
                if notify {
                    if let Some(callback) = &notifier {
                        call_safely(|| {
                            callback(
                                "localflow: meeting detected",
                                &format!(
                                    "Mic in use ({platform}). Open the localflow UI to transcribe."
                                ),
                            )
                        });
                    }
                }
                if let Some(callback) = &on_detect {
                    call_safely(|| callback(&platform));
                }
            }
        });
    }
    pub fn stop(&self) {
        let mut state = self.control.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.status.detected = false;
        state.status.mic_busy = false;
        state.busy_since = None;
        state.notified = false;
        self.control.wake.notify_all();
    }
    pub fn pause(&self) {
        self.control
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .paused = true;
    }
    pub fn resume(&self) {
        let mut state = self.control.state.lock().unwrap_or_else(|e| e.into_inner());
        state.busy_since = None;
        state.status.detected = false;
        state.notified = false;
        state.paused = false;
    }
    pub fn dismiss(&self) {
        self.control
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .detected = false;
    }
}
impl Drop for MeetingWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sustained_busy_blips_dismiss_and_idle_reset() {
        let mut state = State::default();
        let min = Duration::from_secs(12);
        let observe = |state: &mut State, busy, seconds| {
            state.observe(busy, Duration::from_secs(seconds), min)
        };
        assert!(!observe(&mut state, true, 0));
        assert!(!observe(&mut state, false, 11));
        assert!(!observe(&mut state, true, 12));
        assert!(!observe(&mut state, true, 23));
        assert!(observe(&mut state, true, 24));
        assert!(!observe(&mut state, true, 25));
        state.status.detected = false;
        assert!(observe(&mut state, true, 26));
        assert!(!observe(&mut state, false, 27));
        assert!(!state.notified);
    }
}
