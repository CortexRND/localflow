//! The seams the runtime is driven through. A parallel session builds the
//! real `lf-audio`/`lf-stt` crates; these mirror their `SpeechEngine`
//! (load, transcribe(pcm) -> text, unload) so they can be wired in later.

use lf_input::paste::{Injector, SelectionCapture};

/// Mono f32 PCM at the configured sample rate.
pub type Pcm = Vec<f32>;

pub trait SpeechEngine: Send + Sync {
    fn loaded(&self) -> bool {
        true
    }
    fn load(&self) -> Result<(), String>;
    /// `prompt` is the dictionary vocabulary hint (`stt_prompt`), if any.
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String>;
    fn unload(&self);
}

pub struct ReportingSpeech {
    inner: std::sync::Arc<dyn SpeechEngine>,
    error: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
}

impl ReportingSpeech {
    pub fn new(
        inner: std::sync::Arc<dyn SpeechEngine>,
        error: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Self {
        Self { inner, error }
    }
    fn report<T>(&self, result: Result<T, String>) -> Result<T, String> {
        result.inspect_err(|error| (self.error)(error))
    }
}

impl SpeechEngine for ReportingSpeech {
    fn loaded(&self) -> bool {
        self.inner.loaded()
    }
    fn load(&self) -> Result<(), String> {
        self.report(self.inner.load())
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.report(self.inner.transcribe(pcm, prompt))
    }
    fn unload(&self) {
        self.inner.unload();
    }
}

/// The microphone. `start` opens it; `stop` closes it and returns the clip.
pub trait AudioSource: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Pcm;
    fn is_recording(&self) -> bool;
}

/// Clipboard and paste operations the controller performs.
pub trait TextOutput: Send + Sync {
    fn paste_text(&self, text: &str, method: &str, clipboard_before: Option<String>);
    fn capture_selection(&self) -> SelectionCapture;
    fn restore_clipboard_if_matches(&self, expected: &str, clipboard_before: Option<&str>);
}

impl TextOutput for Injector {
    fn paste_text(&self, text: &str, method: &str, clipboard_before: Option<String>) {
        Injector::paste_text(self, text, method, clipboard_before)
    }

    fn capture_selection(&self) -> SelectionCapture {
        Injector::capture_selection(self)
    }

    fn restore_clipboard_if_matches(&self, expected: &str, clipboard_before: Option<&str>) {
        Injector::restore_clipboard_if_matches(self, expected, clipboard_before)
    }
}

/// `audio.rms_dbfs`: -inf for silence or an empty clip.
pub fn rms_dbfs(pcm: &[f32]) -> f64 {
    if pcm.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mean = pcm.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / pcm.len() as f64;
    let rms = mean.sqrt();
    if rms == 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * rms.log10()
}
