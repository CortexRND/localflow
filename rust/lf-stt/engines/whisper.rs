use std::path::PathBuf;
use std::sync::Once;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::{
    downloads,
    model_storage::{whisper_cpp_file, WHISPER_CPP_REPO},
    LoadInfo, SpeechEngine, Timings, Transcript,
};

static LOGGING: Once = Once::new();

pub struct WhisperEngine {
    size: String,
    language: Option<String>,
    provider: String,
    path: Option<PathBuf>,
    context: Option<WhisperContext>,
    /// Built by [`WhisperEngine::cached`]: never fall back to a download.
    cache_only: bool,
}

impl WhisperEngine {
    pub fn new(size: &str, language: Option<String>, provider: &str) -> Result<Self> {
        LOGGING.call_once(whisper_rs::install_logging_hooks);
        if whisper_cpp_file(size).is_none() {
            bail!("unsupported whisper.cpp model size: {size}");
        }
        if !matches!(provider, "auto" | "cpu" | "metal") {
            bail!("unsupported whisper.cpp provider: {provider}");
        }
        if provider == "metal" && !cfg!(target_os = "macos") {
            bail!("Metal requires macOS");
        }
        if let Some(language) = &language {
            if language.contains('\0') || whisper_rs::get_lang_id(language).is_none() {
                bail!("unsupported Whisper language: {language}");
            }
        }
        let provider = if provider == "auto" {
            if cfg!(target_os = "macos") {
                "metal"
            } else {
                "cpu"
            }
        } else {
            provider
        };
        Ok(Self {
            size: size.into(),
            language: language.or_else(|| size.ends_with(".en").then(|| "en".into())),
            provider: provider.into(),
            path: None,
            context: None,
            cache_only: false,
        })
    }

    /// An engine for an already installed model in the HF cache at
    /// `cache_root`. Construction fails if the file isn't there; if it
    /// disappears afterwards, `load` fails instead of downloading it.
    pub fn cached(
        size: &str,
        language: Option<String>,
        provider: &str,
        cache_root: &std::path::Path,
    ) -> Result<Self> {
        let mut engine = Self::new(size, language, provider)?;
        let file = whisper_cpp_file(size).context("unknown ggml size")?;
        engine.path = Some(
            downloads::cached_model_file(cache_root, WHISPER_CPP_REPO, file).with_context(
                || format!("the Whisper {size} model isn't downloaded ({WHISPER_CPP_REPO}/{file})"),
            )?,
        );
        engine.cache_only = true;
        Ok(engine)
    }
}

impl SpeechEngine for WhisperEngine {
    fn id(&self) -> &str {
        "whisper-cpp"
    }
    fn download(&mut self) -> Result<f64> {
        if self
            .path
            .as_ref()
            .is_some_and(|path| downloads::cached_file_exists(path))
        {
            return Ok(0.0);
        }
        if self.cache_only {
            bail!(
                "the Whisper {} model file was removed; it isn't downloaded again here",
                self.size
            );
        }
        let start = Instant::now();
        self.path = Some(downloads::model_file(
            WHISPER_CPP_REPO,
            whisper_cpp_file(&self.size).context("unknown ggml size")?,
        )?);
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }
    fn load(&mut self) -> Result<LoadInfo> {
        let download_ms = self.download()?;
        let start = Instant::now();
        if self.context.is_none() {
            let mut parameters = WhisperContextParameters::default();
            parameters.use_gpu(self.provider == "metal");
            let path = self
                .path
                .as_ref()
                .context("model not downloaded")?
                .to_str()
                .context("Whisper model path is not UTF-8")?;
            self.context = Some(WhisperContext::new_with_params(path, parameters)?);
        }
        Ok(LoadInfo {
            load_ms: start.elapsed().as_secs_f64() * 1000.0,
            download_ms,
            provider: self.provider.clone(),
        })
    }
    fn transcribe(&mut self, pcm: &[f32], prompt: Option<&str>) -> Result<Transcript> {
        let context = self.context.as_ref().context("Whisper is not loaded")?;
        let start = Instant::now();
        let text = if pcm.is_empty() {
            String::new()
        } else {
            if pcm.iter().any(|s| !s.is_finite()) {
                bail!("PCM contains non-finite samples");
            }
            if prompt.is_some_and(|p| p.contains('\0')) {
                bail!("initial prompt contains a NUL byte");
            }
            let mut state = context.create_state()?;
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_n_threads(super::threads());
            params.set_language(self.language.as_deref());
            params.set_no_context(true);
            params.set_temperature(0.0);
            params.set_temperature_inc(0.0);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            if let Some(prompt) = prompt {
                params.set_initial_prompt(prompt);
            }
            state.full(params, pcm)?;
            let segments = state
                .as_iter()
                .map(|s| s.to_str().map(str::to_owned))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            super::join_segments(segments.iter().map(String::as_str))
        };
        Ok(Transcript {
            text,
            timings: Timings {
                audio_seconds: pcm.len() as f64 / 16000.0,
                inference_ms: start.elapsed().as_secs_f64() * 1000.0,
            },
        })
    }
    fn unload(&mut self) {
        self.context = None;
    }
    fn loaded(&self) -> bool {
        self.context.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_settings_and_unloaded_transcription_fail_without_download() {
        assert!(WhisperEngine::new("missing", None, "cpu").is_err());
        assert!(WhisperEngine::new("base", None, "cuda").is_err());
        assert_eq!(
            WhisperEngine::new("base.en", None, "cpu")
                .unwrap()
                .language
                .as_deref(),
            Some("en")
        );
        assert!(WhisperEngine::new("base", None, "cpu")
            .unwrap()
            .language
            .is_none());
        assert!(WhisperEngine::new("base", Some("not-a-language".into()), "cpu").is_err());
        let mut engine = WhisperEngine::new("base", None, "cpu").unwrap();
        assert!(!engine.loaded());
        assert!(engine.transcribe(&[], None).is_err());
    }

    #[test]
    fn cached_engine_never_downloads_a_missing_or_removed_model() {
        let cache = tempfile::tempdir().unwrap();
        let error = WhisperEngine::cached("base", None, "cpu", cache.path())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("isn't downloaded"), "{error}");
        crate::downloads::install_for_test(
            cache.path(),
            WHISPER_CPP_REPO,
            &[("ggml-tiny.bin", b"weights")],
        );
        // Another size in the same repo folder doesn't count as installed.
        assert!(WhisperEngine::cached("base", None, "cpu", cache.path()).is_err());
        let mut engine = WhisperEngine::cached("tiny", None, "cpu", cache.path()).unwrap();
        assert!(engine.path.as_ref().unwrap().starts_with(cache.path()));
        assert_eq!(engine.download().unwrap(), 0.0);
        std::fs::remove_file(engine.path.clone().unwrap()).unwrap();
        // Removed after resolution: download and load refuse without reaching
        // `downloads::model_file` (whose error would say "could not download").
        for error in [
            engine.download().unwrap_err().to_string(),
            engine.load().err().unwrap().to_string(),
        ] {
            assert!(error.contains("isn't downloaded again"), "{error}");
        }
        assert!(!engine.loaded());
    }
}
