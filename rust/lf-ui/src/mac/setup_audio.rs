//! `lf_audio` microphone metering for the native setup window.
//!
//! The recorder lives on a worker. AppKit reads only a cached level and queues
//! stop requests, so opening or closing an audio device cannot block the UI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

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
        #[cfg(feature = "runtime")]
        {
            lf_stt::factory::backend_available(backend)
        }
        #[cfg(not(feature = "runtime"))]
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

enum MeterCommand {
    Start(
        Option<String>,
        Arc<AtomicBool>,
        std::sync::mpsc::Sender<Result<(), String>>,
    ),
    Stop,
}

/// Audio operations are serialized on one worker; AppKit only reads cached RMS.
pub struct RecorderMicLevelSource {
    sender: std::sync::mpsc::Sender<MeterCommand>,
    level: std::sync::Arc<Mutex<Result<f64, String>>>,
}

impl RecorderMicLevelSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for RecorderMicLevelSource {
    fn default() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let level = std::sync::Arc::new(Mutex::new(Err("Microphone meter isn't running.".into())));
        let cached = level.clone();
        let result = std::thread::Builder::new()
            .name("setup-meter".into())
            .spawn(move || {
                let mut recorder: Option<(Recorder, Arc<AtomicBool>)> = None;
                loop {
                    match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(MeterCommand::Start(device, cancelled, reply)) => {
                            if cancelled.load(Ordering::Acquire) {
                                let _ = reply.send(Err("Microphone opening was cancelled.".into()));
                                continue;
                            }
                            if let Some((mut previous, _)) = recorder.take() {
                                let _ = previous.stop();
                            }
                            let result = crate::setup_flow::start_cancellable_capture(
                                &cancelled,
                                || {
                                    let mut next = Recorder::new_meter(device);
                                    next.start().map_err(|error| {
                                        format!("Couldn't open the microphone: {error}")
                                    })?;
                                    Ok(next)
                                },
                                |mut next| {
                                    let _ = next.stop();
                                },
                            )
                            .map(|next| {
                                recorder = Some((next, cancelled.clone()));
                            });
                            *cached.lock().unwrap_or_else(|error| error.into_inner()) =
                                result.clone().map(|()| 0.0);
                            let _ = reply.send(result);
                        }
                        Ok(MeterCommand::Stop) => {
                            if let Some((mut previous, _)) = recorder.take() {
                                let _ = previous.stop();
                            }
                            *cached.lock().unwrap_or_else(|error| error.into_inner()) =
                                Err("Microphone meter isn't running.".into());
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
                    }
                    if recorder
                        .as_ref()
                        .is_some_and(|(_, cancelled)| cancelled.load(Ordering::Acquire))
                    {
                        if let Some((mut previous, _)) = recorder.take() {
                            let _ = previous.stop();
                        }
                    }
                    if let Some((recorder, _)) = &recorder {
                        let measured = normalized_level(recorder.level_dbfs());
                        *cached.lock().unwrap_or_else(|error| error.into_inner()) = measured;
                    }
                }
                if let Some((mut recorder, _)) = recorder {
                    let _ = recorder.stop();
                }
            });
        if let Err(error) = result {
            *level.lock().unwrap() = Err(format!("Couldn't start microphone worker: {error}"));
        }
        Self { sender, level }
    }
}

impl MicLevelSource for RecorderMicLevelSource {
    fn start(&self, device: Option<&str>, cancelled: Arc<AtomicBool>) -> Result<(), String> {
        let (sender, reply) = std::sync::mpsc::channel();
        self.sender
            .send(MeterCommand::Start(
                device.map(str::to_owned),
                cancelled,
                sender,
            ))
            .map_err(|_| "Microphone worker isn't available.".to_string())?;
        reply
            .recv()
            .map_err(|_| "Microphone worker stopped.".to_string())?
    }

    fn level(&self) -> Result<f64, String> {
        self.level
            .lock()
            .map_err(|_| "Microphone level cache is poisoned.".to_string())?
            .clone()
    }

    fn stop(&self) {
        let _ = self.sender.send(MeterCommand::Stop);
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

    #[test]
    fn ui_reads_cached_levels_and_queues_stops_without_waiting_for_audio() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let meter = std::sync::Arc::new(RecorderMicLevelSource {
            sender,
            level: std::sync::Arc::new(Mutex::new(Ok(0.25))),
        });
        assert_eq!(meter.level(), Ok(0.25));
        meter.stop();
        assert!(matches!(receiver.try_recv(), Ok(MeterCommand::Stop)));
        let opening = meter.clone();
        let worker = std::thread::spawn(move || {
            opening.start(Some("Test mic"), Arc::new(AtomicBool::new(false)))
        });
        let MeterCommand::Start(device, _, reply) = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
        else {
            panic!("expected start request");
        };
        assert_eq!(device.as_deref(), Some("Test mic"));
        reply
            .send(Err("Microphone permission denied".into()))
            .unwrap();
        assert_eq!(
            worker.join().unwrap(),
            Err("Microphone permission denied".into())
        );
    }
}
