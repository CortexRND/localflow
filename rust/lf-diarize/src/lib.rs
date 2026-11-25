//! Meeting speaker labels (design §5.3–5.4), the Rust port of
//! `localflow/diarization/{align,cluster}.py` plus the per-meeting collector.
//!
//! Pure by default. The `sherpa` feature adds [`sherpa::SherpaBackend`], which
//! reads the two B6 models from the local Hugging Face cache only.

pub mod align;
pub mod cluster;
pub mod collector;
pub mod models;
#[cfg(feature = "sherpa")]
pub mod sherpa;

pub use align::{assign_speakers, merge_runs, SpeakerRun, GAP_TOLERANCE_S};
pub use cluster::cluster;
pub use collector::{Collector, Finalized, LabelledChunk, Options, PendingLabels, Status};
pub use lf_core::transcript::TimedText;

/// Capture sample rate; every backend call receives 16 kHz mono audio.
pub const SAMPLE_RATE: u32 = 16_000;

/// Chunk-local diarization output; `local_speaker` is only meaningful within the chunk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeakerTurn {
    pub start: f64,
    pub end: f64,
    pub local_speaker: usize,
}

/// A diarizer plus speaker embedder for one meeting, at [`SAMPLE_RATE`].
/// Every method runs on the collector's worker thread, never under the STT lock.
pub trait SpeakerBackend: Send {
    /// Loads local model files only; never touches the network.
    fn load(&mut self) -> anyhow::Result<()>;
    fn diarize(&mut self, audio: &[f32]) -> anyhow::Result<Vec<SpeakerTurn>>;
    fn embed(&mut self, audio: &[f32]) -> anyhow::Result<Vec<f32>>;
}
