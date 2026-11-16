//! `lf_audio` microphone metering for the native setup window.
//!
//! Construction is lazy: the recorder and its audio stream are created only by
//! [`MicLevelSource::start`]. The mutex also serializes a worker opening a
//! stream with main-thread level polling and cancellation.

use std::sync::{Mutex, MutexGuard};

use lf_audio::Recorder;

use super::setup::MicLevelSource;
use crate::setup_flow::{HardwareInfo, HostPlatform, MicAuthorization, SetupPlatform};

pub struct NativeSetupPlatform;

impl SetupPlatform for NativeSetupPlatform {
    fn system(&self) -> String {
        HostPlatform.system()
    }
    fn machine(&self) -> String {
        HostPlatform.machine()
    }
    fn which(&self, command: &str) -> bool {
        HostPlatform.which(command)
    }
    fn ollama_installed(&self) -> bool {
        HostPlatform.ollama_installed()
    }
    fn ollama_running(&self, url: &str) -> Option<bool> {
        HostPlatform.ollama_running(url)
    }
    fn input_devices(&self) -> Result<Vec<String>, String> {
        Ok(lf_audio::list_input_devices())
    }
    fn runtime_available(&self, backend: &str) -> bool {
        HostPlatform.runtime_available(backend)
    }
    fn mic_authorization(&self) -> Option<MicAuthorization> {
        HostPlatform.mic_authorization()
    }
    fn accessibility_trusted(&self) -> Option<bool> {
        HostPlatform.accessibility_trusted()
    }
    fn input_monitoring_granted(&self) -> Option<bool> {
        lf_input::listener::input_monitoring_granted()
    }
    fn detect_hardware(&self) -> Result<HardwareInfo, String> {
        HostPlatform.detect_hardware()
    }
}

/// Thread-safe, lazily opened microphone meter backed by [`Recorder`].
///
/// Starting again stops and drops the previous recorder before opening the new
/// device. Stopping takes the recorder out of the slot, so its stream is closed
/// even when stopping the capture returns an error.
pub struct RecorderMicLevelSource {
    recorder: Mutex<Option<Recorder>>,
}

impl RecorderMicLevelSource {
    /// Create an idle meter without enumerating devices or opening audio
    /// hardware.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<Recorder>>, String> {
        self.recorder
            .lock()
            .map_err(|_| "Microphone recorder lock is poisoned.".to_string())
    }
}

impl Default for RecorderMicLevelSource {
    fn default() -> Self {
        Self {
            recorder: Mutex::new(None),
        }
    }
}

impl MicLevelSource for RecorderMicLevelSource {
    fn start(&self, device: Option<&str>) -> Result<(), String> {
        let mut slot = self.lock()?;
        if let Some(mut previous) = slot.take() {
            previous.stop().map_err(|error| {
                format!("Couldn't stop the previous microphone stream: {error}")
            })?;
        }

        let mut recorder = Recorder::new(device.map(str::to_owned));
        recorder
            .start()
            .map_err(|error| format!("Couldn't open the microphone: {error}"))?;
        *slot = Some(recorder);
        Ok(())
    }

    fn level(&self) -> Result<f64, String> {
        let slot = self.lock()?;
        let recorder = slot
            .as_ref()
            .ok_or_else(|| "Microphone meter isn't running.".to_string())?;
        normalized_level(recorder.level_dbfs())
    }

    fn stop(&self) {
        // Cleanup must still run after another operation poisoned the lock.
        // Unlike start/level, this trait method cannot return that lock error.
        let mut slot = match self.recorder.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                log::error!("microphone recorder lock was poisoned during stop");
                poisoned.into_inner()
            }
        };
        if let Some(mut recorder) = slot.take() {
            if let Err(error) = recorder.stop() {
                log::error!("microphone capture failed while stopping: {error}");
            }
        }
    }
}

/// Match Python's setup meter, which displays linear RMS rather than dBFS.
fn normalized_level(dbfs: f64) -> Result<f64, String> {
    if dbfs == f64::NEG_INFINITY {
        // Recorder uses -infinity both before the first callback and for
        // digital silence. Neither is a device error.
        return Ok(0.0);
    }
    if !dbfs.is_finite() {
        return Err(format!("Microphone returned an invalid dBFS level: {dbfs}"));
    }
    Ok(10.0_f64.powf(dbfs / 20.0).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_dbfs_to_python_compatible_linear_rms() {
        let half = normalized_level(-6.020_599_913_279_624).unwrap();
        assert!((half - 0.5).abs() < 1e-12);
        assert_eq!(normalized_level(0.0).unwrap(), 1.0);
        assert_eq!(normalized_level(6.0).unwrap(), 1.0);

        let quiet = normalized_level(-40.0).unwrap();
        assert!((quiet - 0.01).abs() < 1e-12);
    }

    #[test]
    fn no_samples_or_digital_silence_is_zero() {
        assert_eq!(normalized_level(f64::NEG_INFINITY).unwrap(), 0.0);
    }

    #[test]
    fn invalid_levels_are_errors_not_silent_success() {
        assert!(normalized_level(f64::NAN).is_err());
        assert!(normalized_level(f64::INFINITY).is_err());
    }

    #[test]
    fn adapter_is_send_and_sync_without_audio_hardware() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RecorderMicLevelSource>();
        let meter = RecorderMicLevelSource::new();
        assert!(meter.level().is_err());
        meter.stop();
    }
}
