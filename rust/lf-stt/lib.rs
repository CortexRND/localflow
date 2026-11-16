//! Native speech engines, shared model storage and Python-compatible lease lifecycle.
pub mod downloads;
pub mod engine;
pub mod engines;
pub mod factory;
pub mod model_storage;
pub mod stt_lease;

pub use engine::{EngineSpec, LoadInfo, SpeechEngine, SttSlot, Timings, Transcript};
