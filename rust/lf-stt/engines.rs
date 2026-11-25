#[cfg(feature = "parakeet")]
mod parakeet;
#[cfg(feature = "whisper")]
mod whisper;

#[cfg(feature = "parakeet")]
pub use parakeet::ParakeetEngine;
#[cfg(feature = "whisper")]
pub use whisper::WhisperEngine;

use crate::TimedText;

/// How `WhisperEngine::new` says whisper.cpp has no such model size (a
/// faster-whisper-only size or repo); followed by `: <size>`.
pub const UNSUPPORTED_WHISPER_SIZE: &str = "unsupported whisper.cpp model size";

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

/// Whisper segments as `(start, end, text)` with centisecond times.
pub fn timed_segments<'a>(
    segments: impl IntoIterator<Item = (i64, i64, &'a str)>,
    audio_seconds: f64,
) -> Vec<TimedText> {
    segments
        .into_iter()
        .filter_map(|(start, end, text)| {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            let start = (start as f64 / 100.0).min(audio_seconds).max(0.0);
            let end = (end as f64 / 100.0).min(audio_seconds).max(start);
            Some(TimedText {
                start,
                end,
                text: text.into(),
            })
        })
        .collect()
}

/// Sentence spans from per-token start times (seconds); empty when timing is missing.
pub fn token_segments(
    tokens: &[String],
    timestamps: Option<&[f32]>,
    durations: Option<&[f32]>,
    audio_seconds: f64,
) -> Vec<TimedText> {
    let Some(timestamps) = timestamps.filter(|t| !tokens.is_empty() && t.len() == tokens.len())
    else {
        return Vec::new();
    };
    let tokens: Vec<_> = tokens.iter().map(|t| t.replace('\u{2581}', " ")).collect();
    let mut groups = Vec::new();
    let mut first = 0;
    for (i, token) in tokens.iter().enumerate() {
        let next_spaced = tokens
            .get(i + 1)
            .is_none_or(|next| next.starts_with(char::is_whitespace));
        if i + 1 == tokens.len() || (token.trim().ends_with(['.', '?', '!']) && next_spaced) {
            groups.push((first, i));
            first = i + 1;
        }
    }
    let mut out = Vec::new();
    let mut floor = 0.0;
    for (n, &(first, last)) in groups.iter().enumerate() {
        let text = tokens[first..=last].concat();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let end = match (groups.get(n + 1), durations) {
            (Some(&(next, _)), _) => timestamps[next] as f64,
            (None, Some(d)) if d.len() == tokens.len() => (timestamps[last] + d[last]) as f64,
            (None, _) => audio_seconds,
        };
        let start = (timestamps[first] as f64).min(audio_seconds).max(floor);
        let end = end.min(audio_seconds).max(start);
        floor = end;
        out.push(TimedText {
            start,
            end,
            text: text.into(),
        });
    }
    out
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

    fn span(start: f64, end: f64, text: &str) -> TimedText {
        TimedText {
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn whisper_centiseconds_become_clamped_trimmed_spans() {
        assert!(timed_segments([], 1.0).is_empty());
        assert_eq!(
            timed_segments(
                [
                    (0, 150, " Hello there. "),
                    (150, 200, "  "),
                    (-5, 250, " Next"),
                    (300, 100, "back"),
                    (400, 900, "late."),
                ],
                5.0,
            ),
            [
                span(0.0, 1.5, "Hello there."),
                span(0.0, 2.5, "Next"),
                span(3.0, 3.0, "back"),
                span(4.0, 5.0, "late."),
            ]
        );
    }

    #[test]
    fn tokens_group_into_sentences_with_next_start_as_end() {
        let tokens: Vec<String> = [
            " Hello", " world", ".", " How", " are", " you", "?", " Fine",
        ]
        .map(String::from)
        .into();
        let times = [0.0, 0.4, 0.8, 1.2, 1.5, 1.7, 2.0, 2.6];
        let (a, b) = (1.2f32 as f64, 2.6f32 as f64);
        assert_eq!(
            token_segments(&tokens, Some(&times), None, 3.5),
            [
                span(0.0, a, "Hello world."),
                span(a, b, "How are you?"),
                span(b, 3.5, "Fine"),
            ]
        );
        let durations = [0.1; 8];
        let last = token_segments(&tokens, Some(&times), Some(&durations), 3.5);
        assert_eq!(last[2].end, (2.6f32 + 0.1f32) as f64);
        assert_eq!(
            token_segments(&tokens, Some(&times), Some(&[0.1]), 3.5)[2].end,
            3.5
        );
        let tokens: Vec<String> = [" Pi", " is", " 3", ".", "14", ".", " Next"]
            .map(String::from)
            .into();
        let times = [0.0, 0.3, 0.6, 0.8, 0.9, 1.1, 1.4];
        let n = 1.4f32 as f64;
        assert_eq!(
            token_segments(&tokens, Some(&times), None, 2.0),
            [span(0.0, n, "Pi is 3.14."), span(n, 2.0, "Next")]
        );
        let tokens: Vec<String> = ["3", ".", "14"].map(String::from).into();
        assert_eq!(
            token_segments(&tokens, Some(&[0.0, 0.2, 0.3]), None, 1.0),
            [span(0.0, 1.0, "3.14")]
        );
    }

    #[test]
    fn tokens_without_usable_timing_yield_nothing() {
        let tokens: Vec<String> = vec![" Hi".into(), "!".into()];
        assert!(token_segments(&tokens, None, None, 1.0).is_empty());
        assert!(token_segments(&tokens, Some(&[0.0]), None, 1.0).is_empty());
        assert!(token_segments(&[], Some(&[]), None, 1.0).is_empty());
        assert_eq!(
            token_segments(&[" Hi.".into(), " ".into()], Some(&[0.0, 0.5]), None, 1.0),
            [span(0.0, 0.5, "Hi.")]
        );
        assert!(token_segments(&[" ".into()], Some(&[0.0]), None, 1.0).is_empty());
    }

    #[test]
    fn token_spans_map_sentencepiece_markers_and_stay_clamped_and_monotonic() {
        let tokens: Vec<String> = ["\u{2581}Hi", "!", "\u{2581}So", "\u{2581}long", "."]
            .map(String::from)
            .into();
        let segments = token_segments(&tokens, Some(&[-1.0, 0.5, 0.25, 3.0, 9.0]), None, 2.0);
        assert_eq!(
            segments,
            [span(0.0, 0.25, "Hi!"), span(0.25, 2.0, "So long.")]
        );
        let segments = token_segments(&tokens, Some(&[1.0, 1.1, 0.5, 0.6, 0.7]), None, 2.0);
        assert_eq!(
            segments,
            [span(1.0, 1.0, "Hi!"), span(1.0, 2.0, "So long.")]
        );
        for pair in segments.windows(2) {
            assert!(pair[0].start <= pair[1].start && pair[0].end <= pair[1].end);
        }
    }
}
