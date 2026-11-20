use lf_runtime::engine::{AudioSource, Pcm, SpeechEngine};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

struct State {
    closed: bool,
    recording: bool,
    active: usize,
    last_use: Instant,
    engines: Vec<Weak<ManagedSpeech>>,
}

#[derive(Clone)]
pub(crate) struct SpeechMemory(Arc<Mutex<State>>, Arc<Condvar>);

impl SpeechMemory {
    pub fn new() -> Self {
        Self(
            Arc::new(Mutex::new(State {
                closed: false,
                recording: false,
                active: 0,
                last_use: Instant::now(),
                engines: Vec::new(),
            })),
            Arc::new(Condvar::new()),
        )
    }

    pub fn speech(&self, inner: Arc<dyn SpeechEngine>) -> Arc<dyn SpeechEngine> {
        let engine = Arc::new(ManagedSpeech {
            inner,
            memory: self.clone(),
            retiring: AtomicBool::new(false),
        });
        self.0.lock().unwrap().engines.push(Arc::downgrade(&engine));
        engine
    }

    pub fn audio(&self, inner: Box<dyn AudioSource>) -> Box<dyn AudioSource> {
        Box::new(ManagedAudio {
            inner,
            memory: self.clone(),
        })
    }

    pub fn shutdown(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        while state.active != 0 {
            state = self.1.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        // Unloading stays under the lock, but the upgraded references may be
        // the last ones (their owner released them meanwhile): an engine's
        // destructor can wait for a helper process, so drop them after it.
        let engines: Vec<_> = state
            .engines
            .drain(..)
            .filter_map(|engine| engine.upgrade())
            .collect();
        for engine in &engines {
            engine.inner.unload();
        }
        drop(state);
        drop(engines);
    }

    /// [`Self::poll_holding`], dropping the inspected engines after the
    /// memory lock is released.
    pub fn poll(&self, enabled: bool, timeout: Duration, now: Instant) -> bool {
        let mut inspected = Inspected::default();
        self.poll_holding(enabled, timeout, now, &mut inspected)
    }

    /// Unload idle (or retiring) engines while no recording or inference is
    /// running. The strong references taken to inspect engines go into
    /// `inspected` instead of being dropped under the memory lock: one may be
    /// the last (its owner retired it concurrently), and dropping that can
    /// block on a helper process exit. Callers that poll under another lock
    /// should drop `inspected` only after releasing that lock too.
    pub fn poll_holding(
        &self,
        enabled: bool,
        timeout: Duration,
        now: Instant,
        inspected: &mut Inspected,
    ) -> bool {
        let Ok(mut state) = self.0.try_lock() else {
            return false;
        };
        if state.active != 0 || state.recording {
            return false;
        }
        let due = !enabled
            || (!timeout.is_zero() && now.saturating_duration_since(state.last_use) >= timeout);
        let mut unloaded = false;
        state.engines.retain(|engine| {
            let Some(engine) = engine.upgrade() else {
                return false;
            };
            if (due || engine.retiring.load(Ordering::Acquire)) && engine.inner.loaded() {
                engine.inner.unload();
                unloaded = true;
                log::info!("unloaded idle speech model");
            }
            inspected.0.push(engine);
            true
        });
        if unloaded {
            let relieved = lf_stt::release_idle_memory();
            log::info!("speech allocator pressure relief: {relieved} bytes");
        }
        unloaded
    }
}

/// Strong references [`SpeechMemory::poll_holding`] took to the engines it
/// inspected. Dropping this may destroy engines, so drop it only once no
/// speech, meeting or server lock is held.
#[derive(Default)]
pub(crate) struct Inspected(Vec<Arc<ManagedSpeech>>);

struct Activity(SpeechMemory);
impl Drop for Activity {
    fn drop(&mut self) {
        let mut state = self.0 .0.lock().unwrap();
        state.active -= 1;
        state.last_use = Instant::now();
        drop(state);
        self.0 .1.notify_all();
    }
}

struct ManagedSpeech {
    inner: Arc<dyn SpeechEngine>,
    memory: SpeechMemory,
    retiring: AtomicBool,
}
impl ManagedSpeech {
    fn activity(&self) -> Result<Activity, String> {
        let mut state = self.memory.0.lock().unwrap();
        if state.closed {
            return Err("speech runtime shut down".into());
        }
        state.active += 1;
        Ok(Activity(self.memory.clone()))
    }
}
impl SpeechEngine for ManagedSpeech {
    fn loaded(&self) -> bool {
        self.inner.loaded()
    }
    fn load(&self) -> Result<(), String> {
        let _active = self.activity()?;
        self.inner.load()
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        let _active = self.activity()?;
        self.inner.transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.retiring.store(true, Ordering::Release);
    }
}

struct ManagedAudio {
    inner: Box<dyn AudioSource>,
    memory: SpeechMemory,
}
impl AudioSource for ManagedAudio {
    fn start(&mut self) -> Result<(), String> {
        let mut state = self.memory.0.lock().unwrap();
        if state.closed {
            return Err("speech runtime shut down".into());
        }
        let result = self.inner.start();
        state.recording = self.inner.is_recording();
        state.last_use = Instant::now();
        result
    }
    fn stop(&mut self) -> Pcm {
        let mut state = self.memory.0.lock().unwrap();
        let pcm = self.inner.stop();
        state.recording = self.inner.is_recording();
        state.last_use = Instant::now();
        pcm
    }
    fn is_recording(&self) -> bool {
        self.inner.is_recording()
    }
}
impl Drop for ManagedAudio {
    fn drop(&mut self) {
        if self.inner.is_recording() {
            self.stop();
        }
    }
}

pub(crate) struct RuntimeSettings {
    path: PathBuf,
    signature: Option<(SystemTime, u64)>,
    pub timeout: Duration,
}
impl RuntimeSettings {
    pub fn new(config_path: &Path) -> Self {
        let mut settings = Self {
            path: config_path.with_file_name("runtime.toml"),
            signature: None,
            timeout: Duration::ZERO,
        };
        settings.reload();
        settings
    }
    pub fn reload(&mut self) {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.signature = None;
                self.timeout = Duration::ZERO;
                return;
            }
            Err(error) => {
                log::warn!("runtime settings: {error}");
                return;
            }
        };
        let signature = Some((
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            metadata.len(),
        ));
        if signature == self.signature {
            return;
        }
        self.signature = signature;
        let parsed = (|| -> anyhow::Result<Duration> {
            let data = lf_core::config::parse_toml_file(&self.path)?;
            let value = data
                .get("stt")
                .and_then(|table| table.get("idle_timeout_seconds"));
            match value {
                None => Ok(Duration::ZERO),
                Some(value) => value.as_u64().map(Duration::from_secs).ok_or_else(|| {
                    anyhow::anyhow!("stt.idle_timeout_seconds must be a non-negative integer")
                }),
            }
        })();
        match parsed {
            Ok(timeout) => self.timeout = timeout,
            Err(error) => log::warn!(
                "{}: {error}; retaining previous runtime settings",
                self.path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Engine {
        loaded: AtomicBool,
        loads: AtomicUsize,
        unloads: AtomicUsize,
    }
    impl SpeechEngine for Engine {
        fn loaded(&self) -> bool {
            self.loaded.load(Ordering::SeqCst)
        }
        fn load(&self) -> Result<(), String> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.loaded.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Ok("hello".into())
        }
        fn unload(&self) {
            self.loaded.store(false, Ordering::SeqCst);
            self.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn engine() -> Arc<Engine> {
        Arc::new(Engine {
            loaded: AtomicBool::new(false),
            loads: AtomicUsize::new(0),
            unloads: AtomicUsize::new(0),
        })
    }
    #[test]
    fn admitted_request_finishes_before_shutdown_unloads_and_returns() {
        use std::sync::mpsc;
        let memory = SpeechMemory::new();
        let fake = engine();
        let speech = Arc::new(ManagedSpeech {
            inner: fake.clone(),
            memory: memory.clone(),
            retiring: AtomicBool::new(false),
        });
        memory
            .0
            .lock()
            .unwrap()
            .engines
            .push(Arc::downgrade(&speech));
        let admitted = speech.activity().unwrap();
        let closing = memory.clone();
        let (finish, done) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            closing.shutdown();
            finish.send(()).unwrap();
        });
        while !memory.0.lock().unwrap().closed {
            std::thread::yield_now();
        }
        let premature = done.recv_timeout(Duration::from_millis(20)).is_ok();
        speech.inner.load().unwrap();
        drop(admitted);
        shutdown.join().unwrap();
        assert!(
            !premature,
            "shutdown returned before the admitted inner call"
        );
        assert!(!fake.loaded(), "admitted request reloaded after shutdown");
        assert!(speech.load().is_err());
    }

    #[test]
    fn shutdown_releases_loaded_models_even_with_retained_service_handles() {
        let memory = SpeechMemory::new();
        let fake = engine();
        let speech = memory.speech(fake.clone());
        speech.load().unwrap();
        memory.shutdown();
        memory.shutdown();
        assert!(!speech.loaded());
        assert_eq!(fake.unloads.load(Ordering::SeqCst), 1);
        assert!(speech.load().is_err());
        assert!(speech.transcribe(&[0.5], None).is_err());
    }
    #[test]
    fn disabled_unloads_once_and_enable_is_lazy_idle_zero_is_disabled() {
        let memory = SpeechMemory::new();
        let fake = engine();
        let speech = memory.speech(fake.clone());
        memory.poll(false, Duration::ZERO, Instant::now());
        assert_eq!(fake.unloads.load(Ordering::SeqCst), 0);
        speech.load().unwrap();
        assert!(!memory.poll(
            true,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(86400)
        ));
        memory.poll(false, Duration::ZERO, Instant::now());
        memory.poll(false, Duration::ZERO, Instant::now());
        assert_eq!(fake.unloads.load(Ordering::SeqCst), 1);
        memory.poll(true, Duration::ZERO, Instant::now());
        assert!(!speech.loaded());
        assert_eq!(fake.loads.load(Ordering::SeqCst), 1);
        speech.load().unwrap();
        assert!(speech.loaded());
        assert!(!memory.poll(true, Duration::from_secs(30), Instant::now()));
        assert!(memory.poll(
            true,
            Duration::from_secs(30),
            Instant::now() + Duration::from_secs(31)
        ));
        assert!(!speech.loaded());
    }
    struct Audio(bool);
    impl AudioSource for Audio {
        fn start(&mut self) -> Result<(), String> {
            self.0 = true;
            Ok(())
        }
        fn stop(&mut self) -> Pcm {
            self.0 = false;
            vec![]
        }
        fn is_recording(&self) -> bool {
            self.0
        }
    }
    #[test]
    fn recording_and_inflight_operations_protect_loaded_model() {
        let memory = SpeechMemory::new();
        let fake = engine();
        let speech = memory.speech(fake);
        speech.load().unwrap();
        let mut audio = memory.audio(Box::new(Audio(false)));
        audio.start().unwrap();
        assert!(!memory.poll(false, Duration::ZERO, Instant::now()));
        assert!(speech.loaded());
        audio.stop();
        let active = {
            memory.0.lock().unwrap().active += 1;
            Activity(memory.clone())
        };
        assert!(!memory.poll(false, Duration::ZERO, Instant::now()));
        drop(active);
        assert!(memory.poll(false, Duration::ZERO, Instant::now()));
        assert!(!speech.loaded());
    }
    #[test]
    fn failed_load_and_transcription_release_activity_and_retirement_is_deferred() {
        use std::sync::mpsc;
        for block_load in [true, false] {
            struct Blocking {
                inner: Arc<Engine>,
                entered: mpsc::Sender<()>,
                release: Mutex<mpsc::Receiver<()>>,
                load: bool,
            }
            impl Blocking {
                fn block(&self) -> Result<(), String> {
                    self.entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    Err("native error".into())
                }
            }
            impl SpeechEngine for Blocking {
                fn loaded(&self) -> bool {
                    self.inner.loaded()
                }
                fn load(&self) -> Result<(), String> {
                    if self.load {
                        self.block()
                    } else {
                        self.inner.load()
                    }
                }
                fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
                    self.block().map(|()| String::new())
                }
                fn unload(&self) {
                    self.inner.unload();
                }
            }
            let memory = SpeechMemory::new();
            let fake = engine();
            fake.load().unwrap();
            let (entered, started) = mpsc::channel();
            let (release, receiver) = mpsc::channel();
            let speech = memory.speech(Arc::new(Blocking {
                inner: fake.clone(),
                entered,
                release: Mutex::new(receiver),
                load: block_load,
            }));
            let running = speech.clone();
            let worker = std::thread::spawn(move || {
                if block_load {
                    running.load()
                } else {
                    running.transcribe(&[0.0], None).map(|_| ())
                }
            });
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!memory.poll(false, Duration::ZERO, Instant::now()));
            speech.unload();
            assert!(speech.loaded());
            assert_eq!(fake.unloads.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            assert_eq!(worker.join().unwrap(), Err("native error".into()));
            assert!(memory.poll(true, Duration::ZERO, Instant::now()));
            assert!(!speech.loaded());
        }
    }
    /// A loaded fake engine that pauses (reporting `pause_on`) inside the
    /// memory's critical section, and whose destructor blocks like a helper
    /// process waiting to exit. Each pause and the destructor wait for one
    /// resume; waits time out so a broken run cannot hang the suite.
    struct Doomed {
        events: std::sync::mpsc::Sender<&'static str>,
        resume: Mutex<std::sync::mpsc::Receiver<()>>,
        pause_on: &'static str,
    }
    impl Doomed {
        fn wait(&self) {
            let _ = self
                .resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
        fn pause(&self, at: &'static str) {
            if self.pause_on == at {
                let _ = self.events.send(at);
                self.wait();
            }
        }
    }
    impl SpeechEngine for Doomed {
        fn loaded(&self) -> bool {
            self.pause("loaded");
            true
        }
        fn load(&self) -> Result<(), String> {
            Ok(())
        }
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            Ok(String::new())
        }
        fn unload(&self) {
            self.pause("unload");
        }
    }
    impl Drop for Doomed {
        fn drop(&mut self) {
            let _ = self.events.send("dropping");
            self.wait();
            let _ = self.events.send("dropped");
        }
    }

    /// Resumes the fake; on drop (including a failing test's unwind) it
    /// releases any pause or destructor still waiting.
    struct Resume(std::sync::mpsc::Sender<()>);
    impl Resume {
        fn go(&self) {
            self.0.send(()).unwrap();
        }
    }
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = self.0.send(());
            let _ = self.0.send(());
        }
    }

    fn doomed(
        pause_on: &'static str,
    ) -> (
        Arc<dyn SpeechEngine>,
        std::sync::mpsc::Receiver<&'static str>,
        Resume,
    ) {
        let (events, received) = std::sync::mpsc::channel();
        let (resume, waiting) = std::sync::mpsc::channel();
        let engine = Arc::new(Doomed {
            events,
            resume: Mutex::new(waiting),
            pause_on,
        });
        (engine, received, Resume(resume))
    }

    fn next(events: &std::sync::mpsc::Receiver<&'static str>) -> &'static str {
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("the fake engine never reported")
    }

    /// Runs `access` on another thread; fails instead of hanging if it blocks.
    fn returns<T: Send + 'static>(access: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(access());
        });
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("speech memory stayed locked while an engine was destroyed")
    }

    #[test]
    fn poll_drops_a_concurrently_retired_engine_outside_the_memory_lock() {
        let memory = SpeechMemory::new();
        let (inner, events, resume) = doomed("loaded");
        let speech = memory.speech(inner);
        let other = memory.speech(engine());
        let polling = memory.clone();
        let poller =
            std::thread::spawn(move || polling.poll(false, Duration::ZERO, Instant::now()));
        // The poll holds its upgraded reference under the memory lock...
        assert_eq!(next(&events), "loaded");
        // ...while the server's reaper releases the only other reference.
        drop(speech);
        resume.go();
        // The poll's reference was the last one: the destructor now blocks.
        assert_eq!(next(&events), "dropping");
        let audio_memory = memory.clone();
        returns(move || {
            let mut audio = audio_memory.audio(Box::new(Audio(false)));
            audio.start().unwrap();
            audio.stop();
        });
        let inference = other.clone();
        assert_eq!(
            returns(move || inference.transcribe(&[0.5], None)),
            Ok("hello".into())
        );
        resume.go();
        assert_eq!(next(&events), "dropped");
        assert!(poller.join().unwrap(), "the idle model was unloaded");
    }

    #[test]
    fn shutdown_drops_a_concurrently_retired_engine_outside_the_memory_lock() {
        let memory = SpeechMemory::new();
        let (inner, events, resume) = doomed("unload");
        let speech = memory.speech(inner);
        let closing = memory.clone();
        let shutdown = std::thread::spawn(move || closing.shutdown());
        // Shutdown unloads under the lock through its upgraded reference...
        assert_eq!(next(&events), "unload");
        // ...while the other reference is released concurrently.
        drop(speech);
        resume.go();
        assert_eq!(next(&events), "dropping");
        // The lock is free: late audio and inference are refused, not stuck.
        let audio_memory = memory.clone();
        assert!(returns(move || audio_memory.audio(Box::new(Audio(false))).start()).is_err());
        let late = memory.clone();
        assert!(returns(move || late.speech(engine()).transcribe(&[0.5], None)).is_err());
        resume.go();
        assert_eq!(next(&events), "dropped");
        shutdown.join().unwrap();
    }

    #[test]
    fn sidecar_missing_changed_deleted_invalid_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = RuntimeSettings::new(&dir.path().join("config.toml"));
        assert_eq!(settings.timeout, Duration::ZERO);
        let text = "[stt]\nidle_timeout_seconds = 15\nfuture_key = true\n";
        std::fs::write(&settings.path, text).unwrap();
        settings.reload();
        assert_eq!(settings.timeout, Duration::from_secs(15));
        assert_eq!(std::fs::read_to_string(&settings.path).unwrap(), text);
        std::fs::write(&settings.path, "[stt]\nidle_timeout_seconds = -1\n").unwrap();
        settings.reload();
        assert_eq!(settings.timeout, Duration::from_secs(15));
        std::fs::write(&settings.path, "not toml\n").unwrap();
        settings.reload();
        assert_eq!(settings.timeout, Duration::from_secs(15));
        std::fs::remove_file(&settings.path).unwrap();
        settings.reload();
        assert_eq!(settings.timeout, Duration::ZERO);
    }
}
