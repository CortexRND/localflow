//! Native speech engines, shared model storage and Python-compatible lease lifecycle.
pub mod downloads;
pub mod engine;
pub mod engines;
pub mod factory;
#[cfg(feature = "runtime")]
pub mod helper;
pub mod model_storage;
#[cfg(feature = "runtime")]
pub mod runtime;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "setup")]
pub mod setup_downloads;
pub mod stt_lease;

pub use engine::{EngineSpec, LoadInfo, SpeechEngine, SttSlot, Timings, Transcript};
pub use lf_core::transcript::TimedText;

/// Return unused allocator pages after native speech resources have been dropped.
pub fn release_idle_memory() -> usize {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone asks the system allocator to inspect all its zones;
        // zero requests all available relief without touching live allocations.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}
