use std::path::Path;

use anyhow::{bail, Context, Result};
use rubato::{FftFixedInOut, Resampler};

pub const FIXED_CLIP: &str = "test_wavs/0.wav";

pub struct Clip {
    pub name: String,
    pub original_sample_rate: u32,
    pub pcm: Vec<f32>,
}

pub fn read_clip(path: Option<&Path>) -> Result<Clip> {
    let downloaded;
    let path = match path {
        Some(path) => path,
        None => {
            downloaded = lf_stt::downloads::model_file(
                lf_stt::model_storage::PARAKEET_ONNX_REPO,
                FIXED_CLIP,
            )?;
            &downloaded
        }
    };
    let mut reader = hound::WavReader::open(path).context("could not open WAV")?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        bail!("invalid WAV rate/channels");
    }
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 2_f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    if samples.is_empty() {
        bail!("benchmark WAV is empty");
    }
    let mono: Vec<_> = samples
        .chunks_exact(usize::from(spec.channels))
        .map(|frame| frame[0])
        .collect();
    let pcm = resample(&mono, spec.sample_rate)?;
    if pcm.iter().any(|s| !s.is_finite()) {
        bail!("benchmark WAV contains non-finite samples");
    }
    Ok(Clip {
        name: path.display().to_string(),
        original_sample_rate: spec.sample_rate,
        pcm,
    })
}

fn resample(pcm: &[f32], rate: u32) -> Result<Vec<f32>> {
    if rate == 16000 {
        return Ok(pcm.into());
    }
    let mut resampler = FftFixedInOut::<f32>::new(rate as usize, 16000, 1024, 1)?;
    let wanted = (pcm.len() as f64 * 16000.0 / f64::from(rate)).round() as usize;
    let delay = resampler.output_delay();
    let mut output = Vec::new();
    let mut offset = 0;
    while output.len() < wanted + delay {
        let count = resampler.input_frames_next();
        let mut block = vec![0.0; count];
        let available = count.min(pcm.len().saturating_sub(offset));
        block[..available].copy_from_slice(&pcm[offset..offset + available]);
        offset += available;
        output.extend(resampler.process(&[block], None)?.remove(0));
    }
    Ok(output[delay..delay + wanted].into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wav_resampling_empty_and_invalid() {
        assert_eq!(resample(&[0.0; 4800], 48000).unwrap().len(), 1600);
        assert!(resample(&[], 48000).unwrap().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        assert!(read_clip(Some(&path)).is_err());
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        hound::WavWriter::create(&path, spec)
            .unwrap()
            .finalize()
            .unwrap();
        assert!(read_clip(Some(&path)).is_err());
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..4800 {
            writer.write_sample(8192_i16).unwrap();
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let clip = read_clip(Some(&path)).unwrap();
        assert_eq!(clip.pcm.len(), 1600);
        assert!((clip.pcm[800] - 0.25).abs() < 0.01);
    }
}
