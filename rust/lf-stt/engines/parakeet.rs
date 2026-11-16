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
        })
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
        config.model_config.num_threads = super::threads();
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
                Ok(recognizer) => (recognizer, self.provider.clone()),
                Err(error) if self.provider == "coreml" => {
                    log::warn!("CoreML Parakeet init failed, falling back to CPU: {error}");
                    (self.create("cpu")?, "cpu".into())
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
