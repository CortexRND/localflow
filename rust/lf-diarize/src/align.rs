//! Alignment of STT segments to diarization turns; port of `align.py`.

use crate::{SpeakerTurn, TimedText};
use lf_core::symbols::pycompat as py;

pub const GAP_TOLERANCE_S: f64 = 0.5;
const EPS: f64 = 1e-9;

/// Consecutive same-speaker text, in seconds from meeting (or source) start.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerRun<K> {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub speaker: K,
}

/// `(score, start, index, speaker)`, compared by score, then start, then index.
type Candidate = (f64, f64, usize, usize);

fn better(cand: &Candidate, best: Option<&Candidate>, larger: bool) -> bool {
    let Some(best) = best else { return true };
    if (cand.0 - best.0).abs() > EPS {
        return if larger {
            cand.0 > best.0
        } else {
            cand.0 < best.0
        };
    }
    if (cand.1 - best.1).abs() > EPS {
        return cand.1 < best.1;
    }
    cand.2 < best.2
}

fn assign_one(t: &TimedText, turns: &[SpeakerTurn]) -> Option<usize> {
    // Insertion-ordered like the Python dict, so near-ties are visited in the same order.
    let mut totals: Vec<Candidate> = Vec::new();
    let mut gap_best: Option<Candidate> = None;
    for (i, turn) in turns.iter().enumerate() {
        let overlap = t.end.min(turn.end) - t.start.max(turn.start);
        if overlap > 0.0 {
            let at = match totals.iter().position(|c| c.3 == turn.local_speaker) {
                Some(at) => at,
                None => {
                    totals.push((0.0, turn.start, i, turn.local_speaker));
                    totals.len() - 1
                }
            };
            let entry = &mut totals[at];
            entry.0 += overlap;
            entry.1 = entry.1.min(turn.start);
        } else if -overlap <= GAP_TOLERANCE_S + EPS {
            let cand = ((-overlap).max(0.0), turn.start, i, turn.local_speaker);
            if better(&cand, gap_best.as_ref(), false) {
                gap_best = Some(cand);
            }
        }
    }
    let mut best: Option<Candidate> = None;
    for cand in &totals {
        if better(cand, best.as_ref(), true) {
            best = Some(*cand);
        }
    }
    best.or(gap_best).map(|c| c.3)
}

/// Assigns each text a `local_speaker`; texts are never split and keep `timed` order.
///
/// 1. The speaker with the largest positive overlap summed over its turns; ties go to
///    the speaker whose earliest overlapping turn starts first, then to the one whose
///    first overlapping turn comes first in `turns`.
/// 2. With no overlap: the nearest turn with a gap `<= GAP_TOLERANCE_S`; ties go to the
///    earlier-starting turn, then the earlier turn in `turns`.
/// 3. Otherwise `None`.
pub fn assign_speakers<'a>(
    timed: &'a [TimedText],
    turns: &[SpeakerTurn],
) -> Vec<(&'a TimedText, Option<usize>)> {
    timed.iter().map(|t| (t, assign_one(t, turns))).collect()
}

/// Merges consecutive items with an equal speaker key into one run spanning
/// `chunk_offset` plus the min start and max end of its items; the text is the
/// Python-stripped non-empty texts joined by a space (`""` if all are empty).
pub fn merge_runs<K: Clone + PartialEq>(
    assigned: &[(&TimedText, K)],
    chunk_offset: f64,
) -> Vec<SpeakerRun<K>> {
    let mut runs: Vec<SpeakerRun<K>> = Vec::new();
    let mut start = 0;
    while start < assigned.len() {
        let key = &assigned[start].1;
        let end = assigned[start..]
            .iter()
            .position(|(_, k)| k != key)
            .map_or(assigned.len(), |n| start + n);
        let group = &assigned[start..end];
        let text = group
            .iter()
            .map(|(t, _)| py::strip(&t.text))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        runs.push(SpeakerRun {
            start: chunk_offset
                + group
                    .iter()
                    .map(|(t, _)| t.start)
                    .fold(f64::INFINITY, f64::min),
            end: chunk_offset
                + group
                    .iter()
                    .map(|(t, _)| t.end)
                    .fold(f64::NEG_INFINITY, f64::max),
            text,
            speaker: key.clone(),
        });
        start = end;
    }
    runs
}
