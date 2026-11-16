use super::{
    find_loopback_device, to_mono, AudioCallback, AudioSource, InputDevice, StreamingResampler,
    SystemAudioUnavailable, SAMPLE_RATE,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
};

fn unavailable(error: impl std::fmt::Display) -> SystemAudioUnavailable {
    SystemAudioUnavailable(error.to_string())
}
pub fn input_devices() -> Result<Vec<InputDevice>, SystemAudioUnavailable> {
    let devices = cpal::default_host().input_devices().map_err(unavailable)?;
    devices
        .map(|device| {
            let config = device.default_input_config().map_err(unavailable)?;
            Ok(InputDevice {
                name: device.name().map_err(unavailable)?,
                max_input_channels: config.channels(),
                default_sample_rate: config.sample_rate().0,
            })
        })
        .collect()
}
pub struct CpalSource {
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    sample_rate: u32,
    label: String,
    stream: Option<Stream>,
}
impl CpalSource {
    pub fn microphone() -> Result<Self, SystemAudioUnavailable> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| unavailable("no default input device"))?;
        Self::from_device(device, SAMPLE_RATE, true)
    }
    fn from_device(
        device: Device,
        sample_rate: u32,
        mic: bool,
    ) -> Result<Self, SystemAudioUnavailable> {
        let default = device.default_input_config().map_err(unavailable)?;
        let desired_channels = if mic {
            1
        } else {
            default.channels().clamp(1, 2)
        };
        let desired_rate = if mic {
            SAMPLE_RATE
        } else {
            default.sample_rate().0
        };
        let supported = device
            .supported_input_configs()
            .map_err(unavailable)?
            .find(|c| {
                c.channels() == desired_channels
                    && c.sample_format() == SampleFormat::F32
                    && c.min_sample_rate().0 <= desired_rate
                    && desired_rate <= c.max_sample_rate().0
            })
            .map(|c| c.with_sample_rate(cpal::SampleRate(desired_rate)))
            .unwrap_or(default);
        let label = if mic {
            "microphone".into()
        } else {
            format!("device: {}", device.name().map_err(unavailable)?)
        };
        Ok(Self {
            device,
            config: supported.config(),
            format: supported.sample_format(),
            sample_rate,
            label,
            stream: None,
        })
    }
    fn build<T>(&self, callback: AudioCallback) -> Result<Stream, SystemAudioUnavailable>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let channels = self.config.channels as usize;
        let src_rate = self.config.sample_rate.0;
        let dst_rate = self.sample_rate;
        let mut resampler = StreamingResampler::new(src_rate, dst_rate);
        self.device
            .build_input_stream(
                &self.config,
                move |data: &[T], _| {
                    let audio: Vec<f32> = data.iter().map(|v| v.to_sample::<f32>()).collect();
                    let audio = resampler.process(&to_mono(&audio, channels));
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(audio)))
                        .is_err()
                    {
                        log::warn!("audio callback failed");
                    }
                },
                |_| log::warn!("audio input stream failed"),
                None,
            )
            .map_err(unavailable)
    }
}
impl AudioSource for CpalSource {
    fn start(&mut self, callback: AudioCallback) -> Result<(), SystemAudioUnavailable> {
        if self.stream.is_some() {
            return Ok(());
        }
        let stream = match self.format {
            SampleFormat::I8 => self.build::<i8>(callback),
            SampleFormat::I16 => self.build::<i16>(callback),
            SampleFormat::I32 => self.build::<i32>(callback),
            SampleFormat::I64 => self.build::<i64>(callback),
            SampleFormat::U8 => self.build::<u8>(callback),
            SampleFormat::U16 => self.build::<u16>(callback),
            SampleFormat::U32 => self.build::<u32>(callback),
            SampleFormat::U64 => self.build::<u64>(callback),
            SampleFormat::F32 => self.build::<f32>(callback),
            SampleFormat::F64 => self.build::<f64>(callback),
            other => {
                return Err(unavailable(format!(
                    "unsupported input sample format: {other}"
                )))
            }
        }?;
        stream.play().map_err(unavailable)?;
        self.stream = Some(stream);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), SystemAudioUnavailable> {
        if let Some(stream) = self.stream.take() {
            stream.pause().map_err(unavailable)?;
        }
        Ok(())
    }
    fn label(&self) -> &str {
        &self.label
    }
}
pub(super) fn loopback_source(
    sample_rate: u32,
    name: &str,
) -> Result<Option<CpalSource>, SystemAudioUnavailable> {
    let devices: Vec<_> = match cpal::default_host().input_devices() {
        Ok(devices) => devices.collect(),
        Err(_) => return Ok(None),
    };
    let descriptors: Vec<_> = devices
        .iter()
        .map(|device| {
            let config = device.default_input_config().ok();
            InputDevice {
                name: device.name().unwrap_or_default(),
                max_input_channels: config.as_ref().map_or(0, |c| c.channels()),
                default_sample_rate: config.map_or(sample_rate, |c| c.sample_rate().0),
            }
        })
        .collect();
    find_loopback_device(&descriptors, name)
        .map(|index| {
            CpalSource::from_device(
                devices.into_iter().nth(index).expect("device index"),
                sample_rate,
                false,
            )
        })
        .transpose()
}
