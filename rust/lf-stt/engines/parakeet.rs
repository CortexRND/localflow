use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

use crate::{
    downloads,
    model_storage::{PARAKEET_ONNX_FILES, PARAKEET_ONNX_REPO},
    LoadInfo, SpeechEngine, Timings, Transcript,
};

pub struct ParakeetEngine {
    provider: String,
    paths: Option<Vec<PathBuf>>,
    recognizer: Option<OfflineRecognizer>,
    effective_provider: String,
    num_threads: i32,
    explicit_files: bool,
}

fn provider_report(provider: &str, fallback: bool) -> String {
    if fallback {
        "cpu (fallback)".into()
    } else if provider == "coreml" {
        "coreml requested (unverified)".into()
    } else {
        provider.into()
    }
}

impl ParakeetEngine {
    pub fn new(provider: &str) -> Result<Self> {
        if !matches!(provider, "auto" | "cpu" | "coreml") {
            bail!("unsupported Parakeet provider: {provider}");
        }
        if provider == "coreml" && !cfg!(target_os = "macos") {
            bail!("CoreML requires macOS");
        }
        let provider = if provider == "auto" {
            if cfg!(target_os = "macos") {
                "coreml"
            } else {
                "cpu"
            }
        } else {
            provider
        };
        Ok(Self {
            provider: provider.into(),
            paths: None,
            recognizer: None,
            effective_provider: String::new(),
            num_threads: super::threads(),
            explicit_files: false,
        })
    }

    /// Load an explicitly provisioned native model with a bounded CPU thread count.
    pub fn with_model_files(provider: &str, paths: Vec<PathBuf>, num_threads: i32) -> Result<Self> {
        if paths.len() != 4 || num_threads <= 0 {
            bail!("Parakeet needs four model files and a positive thread count");
        }
        let mut engine = Self::new(provider)?;
        engine.paths = Some(paths);
        engine.num_threads = num_threads;
        engine.explicit_files = true;
        Ok(engine)
    }

    fn create(&self, provider: &str) -> Result<OfflineRecognizer> {
        let paths = self.paths.as_ref().context("Parakeet not downloaded")?;
        let path = |i: usize| {
            paths[i]
                .to_str()
                .map(str::to_owned)
                .context("Parakeet model path is not UTF-8")
        };
        let mut config = OfflineRecognizerConfig::default();
        config.feat_config.sample_rate = 16000;
        config.feat_config.feature_dim = 128;
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: Some(path(0)?),
            decoder: Some(path(1)?),
            joiner: Some(path(2)?),
        };
        config.model_config.tokens = Some(path(3)?);
        config.model_config.num_threads = self.num_threads;
        config.model_config.provider = Some(provider.into());
        config.model_config.model_type = Some("nemo_transducer".into());
        config.decoding_method = Some("greedy_search".into());
        OfflineRecognizer::create(&config).context("could not initialize Parakeet recognizer")
    }
}

impl SpeechEngine for ParakeetEngine {
    fn id(&self) -> &str {
        "parakeet-onnx"
    }
    fn download(&mut self) -> Result<f64> {
        if self
            .paths
            .as_ref()
            .is_some_and(|paths| paths.iter().all(|path| downloads::cached_file_exists(path)))
        {
            return Ok(0.0);
        }
        if self.explicit_files {
            bail!("explicit Parakeet model files are missing");
        }
        let start = Instant::now();
        self.paths = Some(
            PARAKEET_ONNX_FILES
                .iter()
                .map(|f| downloads::model_file(PARAKEET_ONNX_REPO, f))
                .collect::<Result<_>>()?,
        );
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    }
    fn load(&mut self) -> Result<LoadInfo> {
        let download_ms = self.download()?;
        let start = Instant::now();
        if self.recognizer.is_none() {
            let (recognizer, provider) = match self.create(&self.provider) {
                Ok(recognizer) => (recognizer, provider_report(&self.provider, false)),
                Err(error) if self.provider == "coreml" => {
                    log::warn!("CoreML Parakeet init failed, falling back to CPU: {error}");
                    (self.create("cpu")?, provider_report("cpu", true))
                }
                Err(error) => return Err(error),
            };
            self.recognizer = Some(recognizer);
            self.effective_provider = provider;
            if let Err(error) = self.transcribe(&[0.0; 16000], None) {
                self.unload();
                return Err(error);
            }
        }
        Ok(LoadInfo {
            load_ms: start.elapsed().as_secs_f64() * 1000.0,
            download_ms,
            provider: self.effective_provider.clone(),
        })
    }
    fn transcribe(&mut self, pcm: &[f32], _prompt: Option<&str>) -> Result<Transcript> {
        let start = Instant::now();
        let text = if super::parakeet_too_short(pcm) {
            String::new()
        } else {
            if pcm.len() > i32::MAX as usize {
                bail!("Parakeet clip is too long");
            }
            if pcm.iter().any(|s| !s.is_finite()) {
                bail!("PCM contains non-finite samples");
            }
            let recognizer = self.recognizer.as_ref().context("Parakeet is not loaded")?;
            let stream = recognizer.create_stream();
            stream.accept_waveform(16000, pcm);
            recognizer.decode(&stream);
            stream
                .get_result()
                .context("Parakeet returned no result")?
                .text
                .trim()
                .into()
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
        self.recognizer = None;
        self.effective_provider.clear();
    }
    fn loaded(&self) -> bool {
        self.recognizer.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_native_files_validate_without_silent_download_fallback() {
        assert!(ParakeetEngine::with_model_files("cpu", vec![], 2).is_err());
        let root = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["encoder", "decoder", "joiner", "tokens"]
            .iter()
            .map(|name| root.path().join(name))
            .collect();
        assert!(ParakeetEngine::with_model_files("cpu", files.clone(), 0).is_err());
        let mut engine = ParakeetEngine::with_model_files("cpu", files.clone(), 2).unwrap();
        assert!(engine
            .download()
            .unwrap_err()
            .to_string()
            .contains("explicit Parakeet"));
        for path in files {
            std::fs::write(path, b"fixture").unwrap();
        }
        assert_eq!(engine.download().unwrap(), 0.0);
        assert!(!engine.loaded());
    }
    #[test]
    fn provider_reporting_does_not_claim_unobserved_acceleration() {
        assert_eq!(provider_report("cpu", false), "cpu");
        assert_eq!(provider_report("cpu", true), "cpu (fallback)");
        assert_eq!(provider_report("coreml", true), "cpu (fallback)");
        assert_eq!(
            provider_report("coreml", false),
            "coreml requested (unverified)"
        );
    }
    #[test]
    fn short_input_never_runs_or_loads_the_recognizer() {
        let mut engine = ParakeetEngine::new("cpu").unwrap();
        assert!(engine.transcribe(&[], None).unwrap().text.is_empty());
        assert!(engine
            .transcribe(&[0.0; 511], Some("ignored"))
            .unwrap()
            .text
            .is_empty());
        assert!(!engine.loaded());
        assert!(engine.transcribe(&[0.0; 512], None).is_err());
        assert!(ParakeetEngine::new("cuda").is_err());
    }
}
