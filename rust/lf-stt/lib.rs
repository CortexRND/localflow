//! Optional speech model storage and lifecycle; inference engines are not yet implemented.
pub mod engine;
pub mod model_storage;
pub mod stt_lease;

pub use engine::{EngineSpec, LoadInfo, SpeechEngine, SttSlot, Timings, Transcript};
