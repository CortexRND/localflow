//! sherpa-onnx backend: pyannote segmentation for chunk-local turns, then one
//! CAM++ embedding per turn. Loads only the verified, pinned B6 files from the
//! local Hugging Face cache; CPU, single thread, no network.

use crate::{
    models::{verified_path, EMBEDDING, SEGMENTATION},
    SpeakerBackend, SpeakerTurn, SAMPLE_RATE,
};
use anyhow::{anyhow, ensure, Context, Result};
use sherpa_onnx::{
    OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig,
};
use std::path::PathBuf;

/// One thread per model keeps the backend behind STT on small machines (§10 R5).
const NUM_THREADS: i32 = 1;

pub struct SherpaBackend {
    cache: PathBuf,
    diarizer: Option<OfflineSpeakerDiarization>,
    extractor: Option<SpeakerEmbeddingExtractor>,
}

impl SherpaBackend {
    /// `cache` is the Hugging Face hub cache (`lf_stt::model_storage::hf_hub_cache`).
    pub fn new(cache: PathBuf) -> Self {
        Self {
            cache,
            diarizer: None,
            extractor: None,
        }
    }
}

fn embedding_config(model: String) -> SpeakerEmbeddingExtractorConfig {
    SpeakerEmbeddingExtractorConfig {
        model: Some(model),
        num_threads: NUM_THREADS,
        debug: false,
        provider: Some("cpu".into()),
    }
}

impl SpeakerBackend for SherpaBackend {
    fn embedder_id(&self) -> Option<&'static str> {
        Some(EMBEDDING.id)
    }
    fn load(&mut self) -> Result<()> {
        let path = |model| {
            verified_path(&self.cache, model)
                .map_err(|reason| anyhow!(reason))
                .map(|p| p.to_string_lossy().into_owned())
        };
        let segmentation = path(&SEGMENTATION)?;
        let embedding = path(&EMBEDDING)?;
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(segmentation),
                    ..Default::default()
                },
                num_threads: NUM_THREADS,
                debug: false,
                provider: Some("cpu".into()),
            },
            embedding: embedding_config(embedding.clone()),
            ..Default::default()
        };
        let diarizer =
            OfflineSpeakerDiarization::create(&config).context("could not load speaker models")?;
        ensure!(
            diarizer.sample_rate() == SAMPLE_RATE as i32,
            "unexpected diarization sample rate"
        );
        let extractor = SpeakerEmbeddingExtractor::create(&embedding_config(embedding))
            .context("could not load the speaker embedding model")?;
        self.diarizer = Some(diarizer);
        self.extractor = Some(extractor);
        Ok(())
    }

    fn diarize(&mut self, audio: &[f32]) -> Result<Vec<SpeakerTurn>> {
        let diarizer = self
            .diarizer
            .as_ref()
            .context("speaker models not loaded")?;
        let result = diarizer.process(audio).context("diarization failed")?;
        // Segmentation runs over padded windows, so a turn can end past the
        // real audio; clip to it and drop non-finite or empty intervals.
        let duration = audio.len() as f64 / SAMPLE_RATE as f64;
        Ok(result
            .sort_by_start_time()
            .into_iter()
            .filter_map(|s| {
                let (start, end) = (s.start as f64, s.end as f64);
                if !start.is_finite() || !end.is_finite() {
                    return None;
                }
                let (start, end) = (start.max(0.0), end.min(duration));
                (start < end).then_some(SpeakerTurn {
                    start,
                    end,
                    local_speaker: usize::try_from(s.speaker).ok()?,
                })
            })
            .collect())
    }

    fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let extractor = self
            .extractor
            .as_ref()
            .context("speaker models not loaded")?;
        let stream = extractor
            .create_stream()
            .context("embedding stream failed")?;
        stream.accept_waveform(SAMPLE_RATE as i32, audio);
        stream.input_finished();
        ensure!(extractor.is_ready(&stream), "turn too short to embed");
        extractor.compute(&stream).context("embedding failed")
    }
}
