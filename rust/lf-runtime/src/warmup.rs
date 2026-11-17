//! Best-effort cleanup-LLM warm-up at recording start, so a cold Ollama model
//! loads while the user is still speaking instead of after transcription.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_core::config::Config;
use lf_providers::factory::{build_ollama, BuildOptions};

use crate::controller::JobKind;
use crate::settings::{bool_field, str_field};

/// A successful warm-up is trusted this long before another is sent.
pub const WARM_INTERVAL: Duration = Duration::from_secs(60);
/// Matches Ollama's default; the next real request resets it anyway.
pub const WARM_KEEP_ALIVE: &str = "5m";

pub type WarmFn = Arc<dyn Fn(&Config) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct WarmState {
    in_flight: bool,
    last_ok: Option<Instant>,
}

/// Single-flight, throttled warm-ups on a background thread.
#[derive(Clone)]
pub struct LlmWarmer {
    warm: WarmFn,
    interval: Duration,
    state: Arc<Mutex<WarmState>>,
}

/// Whether a job of `kind` started under `config` will use the local Ollama
/// LLM (dictation cleanup, or command mode).
pub fn wants_ollama_warm_up(kind: JobKind, config: &Config) -> bool {
    if str_field(config, "llm_provider") != "ollama"
        || !bool_field(config, "cleanup_enabled", true)
        || str_field(config, "llm_model").trim().is_empty()
    {
        return false;
    }
    let level = str_field(config, "cleanup_level");
    kind == JobKind::Command || (!level.is_empty() && level != "none")
}

/// Load the configured Ollama model without generating.
pub fn ollama_warm_up(config: &Config) -> Result<(), String> {
    build_ollama(config, &BuildOptions::default())
        .ok_or_else(|| "not an Ollama provider".to_string())?
        .warm_up(WARM_KEEP_ALIVE)
        .map_err(|error| error.0)
}

impl LlmWarmer {
    pub fn new(warm: WarmFn) -> Self {
        Self::with_interval(warm, WARM_INTERVAL)
    }

    pub fn with_interval(warm: WarmFn, interval: Duration) -> Self {
        Self {
            warm,
            interval,
            state: Arc::default(),
        }
    }

    pub fn ollama() -> Self {
        Self::new(Arc::new(ollama_warm_up))
    }

    /// Never blocks: spawns a warm-up unless it does not apply, one is in
    /// flight, or one succeeded within the interval. The handle is for tests.
    pub fn on_recording_start(&self, kind: JobKind, config: &Config) -> Option<JoinHandle<()>> {
        if !wants_ollama_warm_up(kind, config) {
            return None;
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let fresh = state.last_ok.is_some_and(|at| at.elapsed() < self.interval);
            if state.in_flight || fresh {
                return None;
            }
            state.in_flight = true;
        }
        let (warm, state, config) = (self.warm.clone(), self.state.clone(), config.clone());
        let spawned = std::thread::Builder::new()
            .name("localflow-llm-warmup".into())
            .spawn(move || {
                let started = Instant::now();
                let result = warm(&config);
                let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                state.in_flight = false;
                match result {
                    Ok(()) => {
                        state.last_ok = Some(Instant::now());
                        log::debug!("cleanup LLM warmed in {}ms", started.elapsed().as_millis());
                    }
                    Err(error) => log::debug!("cleanup LLM warm-up skipped: {error}"),
                }
            });
        match spawned {
            Ok(handle) => Some(handle),
            Err(error) => {
                self.state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .in_flight = false;
                log::debug!("could not start cleanup LLM warm-up: {error}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    fn ollama_config() -> Config {
        let mut config = Config::default();
        config.set_field("llm_provider", serde_json::json!("ollama"));
        config.set_field("llm_model", serde_json::json!("gemma4"));
        config.set_field("cleanup_enabled", serde_json::json!(true));
        config.set_field("cleanup_level", serde_json::json!("light"));
        config
    }

    fn counting(result: Result<(), String>) -> (LlmWarmer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let warmer = LlmWarmer::new(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            result.clone()
        }));
        (warmer, calls)
    }

    #[test]
    fn applies_only_to_enabled_ollama_cleanup() {
        let config = ollama_config();
        assert!(wants_ollama_warm_up(JobKind::Dictate, &config));
        let mut off = config.clone();
        off.set_field("cleanup_enabled", serde_json::json!(false));
        assert!(!wants_ollama_warm_up(JobKind::Dictate, &off));
        assert!(!wants_ollama_warm_up(JobKind::Command, &off));
        let mut none = config.clone();
        none.set_field("cleanup_level", serde_json::json!("none"));
        assert!(!wants_ollama_warm_up(JobKind::Dictate, &none));
        assert!(wants_ollama_warm_up(JobKind::Command, &none));
        let mut remote = config.clone();
        remote.set_field("llm_provider", serde_json::json!("openai-compatible"));
        assert!(!wants_ollama_warm_up(JobKind::Dictate, &remote));
        let mut unnamed = config;
        unnamed.set_field("llm_model", serde_json::json!(""));
        assert!(!wants_ollama_warm_up(JobKind::Dictate, &unnamed));
    }

    #[test]
    fn no_warm_up_when_cleanup_disabled_or_not_ollama() {
        let (warmer, calls) = counting(Ok(()));
        let mut off = ollama_config();
        off.set_field("cleanup_enabled", serde_json::json!(false));
        assert!(warmer.on_recording_start(JobKind::Dictate, &off).is_none());
        let mut remote = ollama_config();
        remote.set_field("llm_provider", serde_json::json!("openai-compatible"));
        assert!(warmer
            .on_recording_start(JobKind::Dictate, &remote)
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn success_is_throttled_and_failure_retries_next_time() {
        let (warmer, calls) = counting(Ok(()));
        let config = ollama_config();
        warmer
            .on_recording_start(JobKind::Dictate, &config)
            .unwrap()
            .join()
            .unwrap();
        assert!(warmer
            .on_recording_start(JobKind::Dictate, &config)
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (failing, calls) = counting(Err("connection refused".into()));
        for _ in 0..2 {
            failing
                .on_recording_start(JobKind::Dictate, &config)
                .unwrap()
                .join()
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let (expired, calls) = counting(Ok(()));
        let expired = LlmWarmer::with_interval(expired.warm, Duration::ZERO);
        for _ in 0..2 {
            expired
                .on_recording_start(JobKind::Dictate, &config)
                .unwrap()
                .join()
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn single_flight_and_never_blocks_the_caller() {
        let (entered, running) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let warmer = LlmWarmer::new(Arc::new(move |_| {
            entered.send(()).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())
        }));
        let config = ollama_config();
        let started = Instant::now();
        let first = warmer
            .on_recording_start(JobKind::Dictate, &config)
            .unwrap();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(warmer
            .on_recording_start(JobKind::Dictate, &config)
            .is_none());
        assert!(warmer
            .on_recording_start(JobKind::Command, &config)
            .is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        release.send(()).unwrap();
        first.join().unwrap();
        assert!(running.try_recv().is_err(), "only one warm-up ran");
    }
}
