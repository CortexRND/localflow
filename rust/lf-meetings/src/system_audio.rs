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
pub struct StreamingResampler {
    src_rate: u64,
    dst_rate: u64,
    phase: u64,
    previous: Option<f32>,
}
impl StreamingResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        assert!(
            src_rate > 0 && dst_rate > 0,
            "sample rates must be positive"
        );
        Self {
            src_rate: src_rate.into(),
            dst_rate: dst_rate.into(),
            phase: src_rate.into(),
            previous: None,
        }
    }
    pub fn process(&mut self, audio: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        for &sample in audio {
            if let Some(previous) = self.previous {
                while self.phase <= self.dst_rate {
                    let fraction = self.phase as f64 / self.dst_rate as f64;
                    output.push(
                        (previous as f64 + fraction * (sample as f64 - previous as f64)) as f32,
                    );
                    self.phase += self.src_rate;
                }
                self.phase -= self.dst_rate;
            } else {
                output.push(sample);
            }
            self.previous = Some(sample);
        }
        output
    }
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

#[cfg(test)]
mod streaming_tests {
    use super::StreamingResampler;

    #[test]
    fn callback_resampling_does_not_accumulate_rounding_drift() {
        let mut resampler = StreamingResampler::new(48000, 16000);
        let frames = [0.25; 512];
        let count: usize = (0..1000)
            .map(|_| {
                let output = resampler.process(&frames);
                assert!(output.iter().all(|&sample| sample == 0.25));
                output.len()
            })
            .sum();
        assert!((count as f64 - 512000.0 / 3.0).abs() <= 1.0, "{count}");
    }

    #[test]
    fn interpolation_is_independent_of_callback_boundaries() {
        let audio: Vec<f32> = (0..2048).map(|n| n as f32).collect();
        for (src, dst) in [
            (48000, 16000),
            (44100, 16000),
            (8000, 16000),
            (16000, 16000),
        ] {
            let expected = StreamingResampler::new(src, dst).process(&audio);
            let mut resampler = StreamingResampler::new(src, dst);
            assert!(resampler.process(&[]).is_empty());
            let actual: Vec<f32> = audio
                .chunks(7)
                .flat_map(|chunk| {
                    assert!(resampler.process(&[]).is_empty());
                    resampler.process(chunk)
                })
                .collect();
            assert_eq!(actual, expected);
            for (i, &sample) in actual.iter().enumerate() {
                assert!((sample as f64 - i as f64 * src as f64 / dst as f64).abs() < 0.001);
            }
        }
    }

    #[test]
    #[should_panic(expected = "sample rates must be positive")]
    fn zero_source_rate_is_rejected() {
        StreamingResampler::new(0, 16000);
    }

    #[test]
    #[should_panic(expected = "sample rates must be positive")]
    fn zero_destination_rate_is_rejected() {
        StreamingResampler::new(48000, 0);
    }
}
