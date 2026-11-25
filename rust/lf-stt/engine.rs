use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::stt_lease::{self, LeaseGuard};
use crate::TimedText;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Timings {
    pub audio_seconds: f64,
    pub inference_ms: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub timings: Timings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<TimedText>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoadInfo {
    pub load_ms: f64,
    pub download_ms: f64,
    pub provider: String,
}

/// Constructors must not load weights. `load` reports success only when ready;
/// `unload` must release resources even following a failed/partial load.
pub trait SpeechEngine: Send {
    fn id(&self) -> &str;
    /// Resolve/download assets without loading weights; milliseconds include cache lookup.
    fn download(&mut self) -> Result<f64> {
        Ok(0.0)
    }
    fn load(&mut self) -> Result<LoadInfo>;
    fn transcribe(&mut self, pcm: &[f32], prompt: Option<&str>) -> Result<Transcript>;
    fn unload(&mut self);
    fn loaded(&self) -> bool;
}

/// Key must uniquely describe all load-affecting settings (model, language,
/// device, etc.). Backend/model are the runtime lease identity.
pub struct EngineSpec {
    pub key: String,
    pub backend: String,
    pub model_size: String,
    pub engine: Box<dyn SpeechEngine>,
}

/// Own one engine and its long-lived process lease. Use one slot per process;
/// temporary parallel work should use scoped leases instead.
#[derive(Default)]
pub struct SttSlot {
    spec: Option<EngineSpec>,
    lease: Option<LeaseGuard>,
    lease_root: Option<PathBuf>,
    scoped_leases: bool,
}

impl SttSlot {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_lease_root(root: PathBuf) -> Self {
        Self {
            lease_root: Some(root),
            spec: None,
            lease: None,
            scoped_leases: false,
        }
    }

    /// Independent runtime snapshots may coexist while a replacement loads.
    /// Scoped leases prevent unloading one snapshot from releasing another's lease.
    pub fn with_scoped_leases(mut self) -> Self {
        self.scoped_leases = true;
        self
    }

    /// Does not load. A different selection releases the old model and lease
    /// immediately; an identical selection leaves the current engine intact.
    pub fn select(&mut self, spec: EngineSpec) {
        if self.spec.as_ref().is_some_and(|old| {
            old.key == spec.key && old.backend == spec.backend && old.model_size == spec.model_size
        }) {
            return;
        }
        self.release();
        self.spec = Some(spec);
    }

    pub fn loaded(&self) -> bool {
        self.spec.as_ref().is_some_and(|s| s.engine.loaded())
    }

    pub fn load(&mut self) -> Result<()> {
        let spec = self
            .spec
            .as_mut()
            .ok_or_else(|| anyhow!("no speech engine selected"))?;
        if !spec.engine.loaded() {
            let backend = stt_lease::selection_backend(Some(&spec.backend));
            let _loading =
                stt_lease::record(backend, &spec.model_size, self.lease_root.as_deref(), true)
                    .ok_or_else(|| anyhow!("could not protect the speech model while loading"))?;
            if let Err(error) = spec.engine.load() {
                spec.engine.unload();
                return Err(error);
            }
            if !spec.engine.loaded() {
                spec.engine.unload();
                bail!("speech engine load succeeded but engine is not loaded");
            }
            self.lease = stt_lease::record(
                &spec.backend,
                &spec.model_size,
                self.lease_root.as_deref(),
                self.scoped_leases,
            );
            if self.lease.is_none() {
                spec.engine.unload();
                bail!("could not record the speech model in use");
            }
        }
        Ok(())
    }

    pub fn transcribe(&mut self, pcm: &[f32], prompt: Option<&str>) -> Result<Transcript> {
        self.load()?;
        self.spec.as_mut().unwrap().engine.transcribe(pcm, prompt)
    }

    /// Release weights and their lease, retaining the selection for a later load.
    pub fn unload(&mut self) {
        if let Some(spec) = self.spec.as_mut() {
            spec.engine.unload();
        }
        self.lease.take();
    }

    pub fn release(&mut self) {
        if let Some(mut spec) = self.spec.take() {
            spec.engine.unload();
            drop(spec);
        }
        self.lease.take();
    }
}

impl Drop for SttSlot {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Counts {
        loads: usize,
        unloads: usize,
    }
    struct Fake {
        counts: Arc<Mutex<Counts>>,
        loaded: bool,
        fail: bool,
    }
    impl SpeechEngine for Fake {
        fn id(&self) -> &str {
            "faster-whisper"
        }
        fn load(&mut self) -> Result<LoadInfo> {
            self.counts.lock().unwrap().loads += 1;
            self.loaded = true;
            if self.fail {
                bail!("load failed");
            }
            Ok(LoadInfo::default())
        }
        fn transcribe(&mut self, pcm: &[f32], prompt: Option<&str>) -> Result<Transcript> {
            if prompt == Some("fail") {
                bail!("inference failed");
            }
            Ok(Transcript {
                text: prompt.unwrap_or("").into(),
                timings: Timings {
                    audio_seconds: pcm.len() as f64 / 16000.0,
                    inference_ms: 0.0,
                },
                segments: Vec::new(),
            })
        }
        fn unload(&mut self) {
            self.counts.lock().unwrap().unloads += 1;
            self.loaded = false;
        }
        fn loaded(&self) -> bool {
            self.loaded
        }
    }
    fn spec(size: &str, counts: &Arc<Mutex<Counts>>, fail: bool) -> EngineSpec {
        EngineSpec {
            key: size.into(),
            backend: "faster-whisper".into(),
            model_size: size.into(),
            engine: Box::new(Fake {
                counts: Arc::clone(counts),
                loaded: false,
                fail,
            }),
        }
    }
    #[test]
    fn lazy_load_switch_release_and_drop_do_not_leak() {
        let root = tempfile::tempdir().unwrap();
        let a = Arc::new(Mutex::new(Counts::default()));
        let b = Arc::new(Mutex::new(Counts::default()));
        let weak = Arc::downgrade(&a);
        let mut slot = SttSlot::with_lease_root(root.path().into());
        assert!(slot.transcribe(&[], None).is_err());
        slot.select(spec("base", &a, false));
        assert_eq!(a.lock().unwrap().loads, 0);
        assert!(!slot.loaded());
        assert_eq!(
            slot.transcribe(&[0.0; 160], Some("hello")).unwrap().text,
            "hello"
        );
        assert_eq!(stt_lease::live_leases(Some(root.path()), |_| true).len(), 1);
        slot.transcribe(&[], None).unwrap();
        slot.select(spec("base", &a, false));
        assert_eq!(a.lock().unwrap().loads, 1);
        assert_eq!(Arc::strong_count(&a), 2);
        slot.select(spec("small", &b, false));
        assert_eq!(a.lock().unwrap().unloads, 1);
        assert_eq!(Arc::strong_count(&a), 1);
        drop(a);
        assert!(weak.upgrade().is_none());
        assert!(stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
        assert_eq!(b.lock().unwrap().loads, 0);
        slot.transcribe(&[], None).unwrap();
        assert!(slot.transcribe(&[], Some("fail")).is_err());
        assert!(slot.loaded());
        drop(slot);
        assert_eq!(b.lock().unwrap().unloads, 1);
        assert_eq!(Arc::strong_count(&b), 1);
        assert!(stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
    }
    #[test]
    fn failed_load_and_failed_lease_are_cleaned_up() {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Mutex::new(Counts::default()));
        let mut slot = SttSlot::with_lease_root(root.path().into());
        slot.select(spec("base", &counts, true));
        assert_eq!(
            slot.transcribe(&[], None).unwrap_err().to_string(),
            "load failed"
        );
        assert!(!slot.loaded());
        assert!(stt_lease::live_leases(Some(root.path()), |_| true).is_empty());
        slot.release();
        assert_eq!(Arc::strong_count(&counts), 1);
        let not_dir = root.path().join("file");
        std::fs::write(&not_dir, "x").unwrap();
        let mut slot = SttSlot::with_lease_root(not_dir);
        slot.select(spec("base", &counts, false));
        assert!(slot.transcribe(&[], None).is_err());
        assert!(!slot.loaded());
    }

    #[test]
    fn failed_runtime_lease_unloads_the_successfully_loaded_engine() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(format!("{}.json", std::process::id()))).unwrap();
        let counts = Arc::new(Mutex::new(Counts::default()));
        let mut slot = SttSlot::with_lease_root(root.path().into());
        slot.select(spec("base", &counts, false));
        assert_eq!(
            slot.transcribe(&[], None).unwrap_err().to_string(),
            "could not record the speech model in use"
        );
        assert!(!slot.loaded());
        assert_eq!(counts.lock().unwrap().loads, 1);
        assert_eq!(counts.lock().unwrap().unloads, 1);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        slot.release();
        assert_eq!(Arc::strong_count(&counts), 1);
    }

    #[test]
    fn untimed_transcript_json_is_unchanged() {
        let json = serde_json::to_value(Transcript::default()).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["text", "timings"]);
        let parsed: Transcript = serde_json::from_str(
            r#"{"text":"hi","timings":{"audio_seconds":1.0,"inference_ms":2.0}}"#,
        )
        .unwrap();
        assert_eq!(parsed.text, "hi");
        assert!(parsed.segments.is_empty());
        let timed = Transcript {
            segments: vec![TimedText {
                start: 0.0,
                end: 1.0,
                text: "hi".into(),
            }],
            ..Transcript::default()
        };
        let json = serde_json::to_string(&timed).unwrap();
        let back: Transcript = serde_json::from_str(&json).unwrap();
        assert_eq!(back.segments, timed.segments);
    }
}
