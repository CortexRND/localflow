//! Microphone capture and Python-compatible dictation gates.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use rubato::{FftFixedIn, Resampler};

pub const SAMPLE_RATE: u32 = 16_000;
pub const MIN_CLIP_SECONDS: f64 = 0.3;
pub const DEFAULT_SILENCE_THRESHOLD_DBFS: f64 = -50.0;

pub fn rms_dbfs(pcm: &[f32]) -> f64 {
    if pcm.is_empty() {
        return f64::NEG_INFINITY;
    }
    let rms = (pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
    if rms == 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * rms.log10()
    }
}

pub fn is_silent(pcm: &[f32], threshold_dbfs: f64) -> bool {
    rms_dbfs(pcm) < threshold_dbfs
}

pub fn clip_too_short(len: usize, sample_rate: u32, min_seconds: f64, force: bool) -> bool {
    let duration = if sample_rate == 0 {
        0.0
    } else {
        len as f64 / f64::from(sample_rate)
    };
    !force && duration < min_seconds
}

pub fn list_input_devices() -> Vec<String> {
    let Ok(devices) = cpal::default_host().input_devices() else {
        return Vec::new();
    };
    let mut names: Vec<_> = devices
        .filter(|d| {
            d.supported_input_configs()
                .map(|mut c| c.any(|c| c.channels() > 0))
                .unwrap_or(false)
        })
        .filter_map(|d| d.name().ok())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

// Callbacks only convert and collect; resampling is done off the audio thread.
struct CaptureBuffer {
    channels: usize,
    rate: u32,
    pcm: Vec<f32>,
}

impl CaptureBuffer {
    fn push<T: Sample>(&mut self, data: &[T], level: &AtomicU64)
    where
        f32: FromSample<T>,
    {
        let start = self.pcm.len();
        self.pcm.extend(
            data.chunks_exact(self.channels)
                .map(|frame| frame[0].to_sample::<f32>()),
        );
        level.store(rms_dbfs(&self.pcm[start..]).to_bits(), Ordering::Relaxed);
    }

    fn finish(self) -> Result<Vec<f32>> {
        if self.rate == SAMPLE_RATE || self.pcm.is_empty() {
            return Ok(self.pcm);
        }
        let mut resampler =
            FftFixedIn::<f32>::new(self.rate as usize, SAMPLE_RATE as usize, 1024, 2, 1)?;
        let delay = resampler.output_delay();
        let wanted =
            (self.pcm.len() as u64 * u64::from(SAMPLE_RATE) / u64::from(self.rate)) as usize;
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < self.pcm.len() || output.len() < delay + wanted {
            let size = resampler.input_frames_next();
            let mut block = vec![0.0; size];
            let count = size.min(self.pcm.len() - offset);
            block[..count].copy_from_slice(&self.pcm[offset..offset + count]);
            offset += count;
            output.extend(resampler.process(&[block], None)?.remove(0));
        }
        Ok(output[delay..delay + wanted].to_vec())
    }
}

struct Active {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<Result<Vec<f32>>>,
}

/// Captures channel zero at 16 kHz. Pass `dictation.input_device` to `new`.
/// Empty/None selects the default device. A second start is an error.
/// The dedicated owner thread drops the stream before stop/drop joins it.
pub struct Recorder {
    device: Option<String>,
    active: Option<Active>,
    level: Arc<AtomicU64>,
}

impl Recorder {
    pub fn new(device: Option<String>) -> Self {
        Self {
            device: device.filter(|s| !s.is_empty()),
            active: None,
            level: Arc::new(AtomicU64::new(f64::NEG_INFINITY.to_bits())),
        }
    }

    pub fn recording(&self) -> bool {
        self.active.is_some()
    }

    /// RMS dBFS of the latest native callback's channel zero, or -inf before capture.
    pub fn level_dbfs(&self) -> f64 {
        f64::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn start(&mut self) -> Result<()> {
        if self.recording() {
            bail!("recorder is already recording");
        }
        self.level
            .store(f64::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        let (stop, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let name = self.device.clone();
        let level = Arc::clone(&self.level);
        let thread = thread::spawn(move || {
            let opened = open_stream(name.as_deref(), level);
            match opened {
                Ok((stream, buffer)) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = receiver.recv();
                    drop(stream);
                    let buffer = Arc::try_unwrap(buffer)
                        .map_err(|_| anyhow!("capture callback still owns buffer"))?
                        .into_inner()
                        .map_err(|_| anyhow!("capture buffer poisoned"))?;
                    buffer.finish()
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    Ok(Vec::new())
                }
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.active = Some(Active { stop, thread });
                Ok(())
            }
            status => {
                let _ = thread.join();
                match status {
                    Ok(Err(e)) => Err(e),
                    _ => Err(anyhow!("capture thread exited during startup")),
                }
            }
        }
    }

    /// Closes the mic and returns the full clip; idle stop returns an empty clip.
    pub fn stop(&mut self) -> Result<Vec<f32>> {
        let Some(active) = self.active.take() else {
            return Ok(Vec::new());
        };
        let _ = active.stop.send(());
        let pcm = active
            .thread
            .join()
            .map_err(|_| anyhow!("capture thread panicked"))??;
        let peak = pcm.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
        log::info!(
            "recording stopped: {:.2}s captured, peak amplitude {:.4}",
            pcm.len() as f64 / f64::from(SAMPLE_RATE),
            peak
        );
        Ok(pcm)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn build_stream<T: SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buffer: Arc<Mutex<CaptureBuffer>>,
    level: Arc<AtomicU64>,
) -> Result<cpal::Stream>
where
    f32: FromSample<T>,
{
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            if let Ok(mut buffer) = buffer.lock() {
                buffer.push(data, &level);
            }
        },
        |error| log::error!("mic input stream error: {error}"),
        None,
    )?)
}

fn open_stream(
    name: Option<&str>,
    level: Arc<AtomicU64>,
) -> Result<(cpal::Stream, Arc<Mutex<CaptureBuffer>>)> {
    let host = cpal::default_host();
    let device = if let Some(name) = name {
        host.input_devices()?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .with_context(|| format!("input device not found: {name}"))?
    } else {
        host.default_input_device()
            .context("no default input device available")?
    };
    let supported = device
        .default_input_config()
        .context("input device has no usable default configuration")?;
    let config: cpal::StreamConfig = supported.clone().into();
    let buffer = Arc::new(Mutex::new(CaptureBuffer {
        channels: config.channels as usize,
        rate: config.sample_rate.0,
        pcm: Vec::new(),
    }));
    macro_rules! build {
        ($t:ty) => {
            build_stream::<$t>(&device, &config, Arc::clone(&buffer), level)
        };
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        format => bail!("unsupported input sample format: {format}"),
    }?;
    stream.play()?;
    log::info!("opening input stream on device: {}", device.name()?);
    Ok((stream, buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(rate: u32, channels: usize) -> CaptureBuffer {
        CaptureBuffer {
            rate,
            channels,
            pcm: Vec::new(),
        }
    }

    #[test]
    fn rms_and_gates() {
        assert_eq!(rms_dbfs(&[]), f64::NEG_INFINITY);
        assert_eq!(rms_dbfs(&[0.0; 4]), f64::NEG_INFINITY);
        assert_eq!(rms_dbfs(&[0.5; 4]), -6.020599913279624);
        assert!(is_silent(&[], DEFAULT_SILENCE_THRESHOLD_DBFS));
        assert!(is_silent(&[0.0; 4], -50.0));
        assert!(!is_silent(&[0.5; 4], rms_dbfs(&[0.5; 4])));
        assert!(!is_silent(&[0.5; 4], -50.0));
        assert!(clip_too_short(4799, SAMPLE_RATE, MIN_CLIP_SECONDS, false));
        assert!(!clip_too_short(4800, SAMPLE_RATE, MIN_CLIP_SECONDS, false));
        assert!(!clip_too_short(4801, SAMPLE_RATE, MIN_CLIP_SECONDS, false));
        assert!(clip_too_short(0, 0, MIN_CLIP_SECONDS, false));
        assert!(!clip_too_short(0, 0, MIN_CLIP_SECONDS, true));
    }

    #[test]
    fn conversions_channel_zero_and_meter() {
        let level = AtomicU64::new(0);
        let mut b = buffer(SAMPLE_RATE, 2);
        b.push(&[16384_i16, -32768, -16384, 32767], &level);
        assert_eq!(b.pcm, [0.5, -0.5]);
        assert_eq!(
            f64::from_bits(level.load(Ordering::Relaxed)),
            rms_dbfs(&[0.5])
        );
        assert_eq!(b.finish().unwrap(), [0.5, -0.5]);
        let mut b = buffer(SAMPLE_RATE, 1);
        b.push(&[0_u16, 32768, 65535], &level);
        assert_eq!(b.pcm, [-1.0, 0.0, 32767.0 / 32768.0]);
        macro_rules! signed { ($($t:ty),*) => { $( {
            let mut b = buffer(SAMPLE_RATE, 1);
            b.push(&[<$t>::MIN, 0, <$t>::MAX], &level);
            assert_eq!(b.pcm[0], -1.0);
            assert_eq!(b.pcm[1], 0.0);
            assert!((b.pcm[2] - 1.0).abs() <= 1.0 / 128.0);
        } )* }; }
        signed!(i8, i32, i64);
        macro_rules! unsigned { ($($t:ty),*) => { $( {
            let mut b = buffer(SAMPLE_RATE, 1);
            b.push(&[<$t>::MIN, <$t>::MAX / 2 + 1, <$t>::MAX], &level);
            assert_eq!(b.pcm[0], -1.0);
            assert_eq!(b.pcm[1], 0.0);
            assert!((b.pcm[2] - 1.0).abs() <= 1.0 / 128.0);
        } )* }; }
        unsigned!(u8, u32, u64);
        let mut b = buffer(SAMPLE_RATE, 2);
        b.push(&[0.25_f64, 0.9, -0.5, 0.9], &level);
        assert_eq!(b.pcm, [0.25, -0.5]);
        b.push::<f32>(&[], &level);
        assert_eq!(
            f64::from_bits(level.load(Ordering::Relaxed)),
            f64::NEG_INFINITY
        );
    }

    #[test]
    fn resampler_length_sine_and_callback_chunk_invariance() {
        let level = AtomicU64::new(0);
        for rate in [8000, 16000, 44100, 48000] {
            assert!(buffer(rate, 1).finish().unwrap().is_empty());
            let native: Vec<f32> = (0..rate)
                .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin())
                .collect();
            let mut whole = buffer(rate, 1);
            whole.push(&native, &level);
            let output = whole.finish().unwrap();
            assert_eq!(output.len(), 16000);
            let mut chunked = buffer(rate, 1);
            for chunk in native.chunks(137) {
                chunked.push(chunk, &level);
            }
            assert_eq!(output, chunked.finish().unwrap());
            let interior = &output[1000..15000];
            let crossings = interior
                .windows(2)
                .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
                .count();
            let hz = crossings as f64 * 16000.0 / interior.len() as f64;
            assert!((hz - 1000.0).abs() < 2.0, "{rate}: {hz}");
            assert!((rms_dbfs(interior) - rms_dbfs(&native)).abs() < 0.1);
        }
        let mut short = buffer(48000, 1);
        short.push(&[0.5_f32; 7], &level);
        assert_eq!(short.finish().unwrap().len(), 2);
    }

    #[test]
    fn idle_recorder_and_double_start_error() {
        let mut r = Recorder::new(Some(String::new()));
        assert!(r.device.is_none());
        assert!(!r.recording());
        assert_eq!(r.level_dbfs(), f64::NEG_INFINITY);
        assert!(r.stop().unwrap().is_empty());
        let (stop, receiver) = mpsc::channel();
        r.active = Some(Active {
            stop,
            thread: thread::spawn(move || {
                let _ = receiver.recv();
                Ok(vec![])
            }),
        });
        assert!(r
            .start()
            .unwrap_err()
            .to_string()
            .contains("already recording"));
        drop(r);
    }

    /// Run `cargo test -p lf-audio -- --ignored` on a machine with a mic.
    #[test]
    #[ignore]
    fn default_mic_releases_on_stop_and_drop() {
        let mut r = Recorder::new(None);
        for _ in 0..2 {
            r.start().unwrap();
            assert!(r.recording());
            thread::sleep(std::time::Duration::from_secs(1));
            let pcm = r.stop().unwrap();
            assert!(!pcm.is_empty());
            assert!(!r.recording());
        }
        r.start().unwrap();
        drop(r);
        let mut reopened = Recorder::new(None);
        reopened.start().unwrap();
        reopened.stop().unwrap();
    }
}
