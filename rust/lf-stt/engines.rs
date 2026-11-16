#[cfg(feature = "parakeet")]
mod parakeet;
#[cfg(feature = "whisper")]
mod whisper;

#[cfg(feature = "parakeet")]
pub use parakeet::ParakeetEngine;
#[cfg(feature = "whisper")]
pub use whisper::WhisperEngine;

/// The original Parakeet model preprocessor uses n_fft=512 at 16 kHz.
pub const PARAKEET_MIN_SAMPLES: usize = 512;

pub fn parakeet_too_short(pcm: &[f32]) -> bool {
    pcm.len() < PARAKEET_MIN_SAMPLES
}

pub fn join_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    segments
        .into_iter()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .into()
}

#[cfg(any(feature = "whisper", feature = "parakeet"))]
fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4) as i32)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_input_and_segment_joining() {
        assert!(parakeet_too_short(&[]));
        assert!(parakeet_too_short(&[0.0; 511]));
        assert!(!parakeet_too_short(&[0.0; 512]));
        assert_eq!(join_segments([]), "");
        assert_eq!(join_segments([" ", ""]), "");
        assert_eq!(join_segments([" hello ", " world. "]), "hello world.");
        assert_eq!(join_segments(["hello", "", "world"]), "hello  world");
    }
}
