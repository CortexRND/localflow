use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 16_000;
pub const AUTO_DEVICE_HINTS: &[&str] = &[
    "blackhole",
    "loopback",
    ".monitor",
    "monitor of",
    "stereo mix",
];
pub const SCK_RATES: &[u32] = &[8000, 16000, 24000, 48000];
pub type AudioCallback = Arc<dyn Fn(Vec<f32>) + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemAudioUnavailable(pub String);
impl std::fmt::Display for SystemAudioUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for SystemAudioUnavailable {}
pub trait AudioSource {
    fn start(&mut self, callback: AudioCallback) -> Result<(), SystemAudioUnavailable>;
    fn stop(&mut self) -> Result<(), SystemAudioUnavailable>;
    fn label(&self) -> &str;
}

pub fn resample(audio: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    assert!(
        src_rate > 0 && dst_rate > 0,
        "sample rates must be positive"
    );
    if src_rate == dst_rate || audio.is_empty() {
        return audio.to_vec();
    }
    let n = (audio.len() as f64 * dst_rate as f64 / src_rate as f64)
        .round_ties_even()
        .max(1.0) as usize;
    (0..n)
        .map(|i| {
            let position = i as f64 * src_rate as f64 / dst_rate as f64;
            let left = (position.floor() as usize).min(audio.len() - 1);
            let right = (left + 1).min(audio.len() - 1);
            let fraction = position - left as f64;
            (audio[left] as f64 + fraction * (audio[right] as f64 - audio[left] as f64)) as f32
        })
        .collect()
}
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    assert!(channels > 0, "channel count must be positive");
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}
#[derive(Clone, Debug)]
pub struct InputDevice {
    pub name: String,
    pub max_input_channels: u16,
    pub default_sample_rate: u32,
}
pub fn find_loopback_device(devices: &[InputDevice], name: &str) -> Option<usize> {
    let named = name.to_lowercase();
    let named = [named.as_str()];
    let needles = if name.is_empty() {
        AUTO_DEVICE_HINTS
    } else {
        &named
    };
    for needle in needles {
        if let Some(index) = devices
            .iter()
            .position(|d| d.max_input_channels > 0 && d.name.to_lowercase().contains(needle))
        {
            return Some(index);
        }
    }
    None
}
pub fn open_system_audio(
    sample_rate: u32,
    device: &str,
) -> Result<Box<dyn AudioSource>, SystemAudioUnavailable> {
    #[cfg(target_os = "macos")]
    if device.is_empty() {
        return Ok(Box::new(crate::macos_audio::ScreenCaptureKitSource::new(
            sample_rate,
        )));
    }
    #[cfg(feature = "capture")]
    if let Some(source) = cpal_source::loopback_source(sample_rate, device)? {
        return Ok(Box::new(source));
    }
    #[cfg(not(feature = "capture"))]
    let _ = sample_rate;
    if !device.is_empty() {
        return Err(SystemAudioUnavailable(format!(
            "no input device matching {}",
            lf_core::symbols::pycompat::repr_str(device)
        )));
    }
    let hint = if cfg!(target_os = "macos") {
        "install the ScreenCaptureKit bindings (pip install -e '.[system-audio]') or set up BlackHole and set [meetings] system_audio_device"
    } else {
        "set [meetings] system_audio_device to a loopback/monitor input device"
    };
    Err(SystemAudioUnavailable(format!(
        "no system audio source found: {hint}"
    )))
}

#[cfg(feature = "capture")]
#[path = "cpal_audio.rs"]
mod cpal_source;
#[cfg(target_os = "macos")]
pub use crate::macos_audio::CoreAudioMicUsageProbe;
#[cfg(target_os = "macos")]
pub use crate::macos_audio::ScreenCaptureKitSource;
#[cfg(feature = "capture")]
pub use cpal_source::{input_devices, CpalSource};
