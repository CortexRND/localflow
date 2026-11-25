use anyhow::{bail, Result};
use lf_diarize::{
    collector::{max_speakers, MAX_SPEAKERS_CAP, QUEUE_CHUNKS},
    Collector, Options, SpeakerBackend, SpeakerRun, SpeakerTurn, Status, TimedText, SAMPLE_RATE,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

/// Turns are encoded in the audio: each 1.5 s block is one turn whose speaker is
/// its amplitude bucket; the embedding is a one-hot of that bucket.
#[derive(Default)]
struct Fake {
    fail_load: bool,
    fail_diarize: bool,
    /// Panics on this (1-based) chunk.
    panic_at: Option<usize>,
    /// Blocks the worker before each chunk until released.
    gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
    chunks: Arc<AtomicUsize>,
    /// Signalled when the worker drops the backend (it has decided cluster or cancel).
    _dropped: Option<DropSignal>,
}
struct DropSignal(mpsc::Sender<()>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
const BLOCK: f64 = 1.5;
fn bucket(sample: f32) -> usize {
    (sample * 10.0).round() as usize
}
impl SpeakerBackend for Fake {
    fn load(&mut self) -> Result<()> {
        if self.fail_load {
            bail!("models missing");
        }
        Ok(())
    }
    fn diarize(&mut self, audio: &[f32]) -> Result<Vec<SpeakerTurn>> {
        if let Some(gate) = &self.gate {
            gate.lock().unwrap().recv().unwrap();
        }
        let chunk = self.chunks.fetch_add(1, Ordering::SeqCst) + 1;
        if self.panic_at == Some(chunk) {
            panic!("backend crashed");
        }
        if self.fail_diarize {
            bail!("onnx error");
        }
        let block = (BLOCK * SAMPLE_RATE as f64) as usize;
        Ok(audio
            .chunks(block)
            .enumerate()
            .map(|(i, samples)| SpeakerTurn {
                start: i as f64 * BLOCK,
                end: i as f64 * BLOCK + samples.len() as f64 / SAMPLE_RATE as f64,
                local_speaker: bucket(samples[0]),
            })
            .collect())
    }
    fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let mut v = vec![0.0; 8];
        v[bucket(audio[0])] = 2.0;
        Ok(v)
    }
}

fn audio(levels: &[f32]) -> Vec<f32> {
    levels
        .iter()
        .flat_map(|&l| std::iter::repeat_n(l, (BLOCK * SAMPLE_RATE as f64) as usize))
        .collect()
}
fn texts(n: usize) -> Vec<TimedText> {
    (0..n)
        .map(|i| TimedText {
            start: i as f64 * BLOCK,
            end: (i + 1) as f64 * BLOCK,
            text: format!(" t{i} "),
        })
        .collect()
}
fn start(fake: Fake) -> Collector {
    Collector::start(Box::new(fake), Options::default()).unwrap()
}
fn run(start: f64, end: f64, text: &str, speaker: Option<usize>) -> SpeakerRun<Option<usize>> {
    SpeakerRun {
        start,
        end,
        text: text.into(),
        speaker,
    }
}

#[test]
fn clusters_across_chunks_per_source_and_aligns_runs() {
    let collector = start(Fake::default());
    assert!(collector.feed("others", 0.0, audio(&[0.1, 0.1, 0.3]), texts(3)));
    assert!(collector.feed("others", 4.5, audio(&[0.3, 0.1]), texts(2)));
    // One speaker on the mic: stays channel-labelled.
    assert!(collector.feed("me", 0.0, audio(&[0.5, 0.5]), texts(2)));
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Ok);
    assert_eq!(result.chunks.len(), 2);
    assert!(result.chunks.iter().all(|c| c.source == "others"));
    assert_eq!(
        result.chunks[0].runs,
        [
            run(0.0, 3.0, "t0 t1", Some(0)),
            run(3.0, 4.5, "t2", Some(1))
        ]
    );
    assert_eq!(result.chunks[1].offset, 4.5);
    assert_eq!(
        result.chunks[1].runs,
        [run(4.5, 6.0, "t0", Some(1)), run(6.0, 7.5, "t1", Some(0))]
    );
    assert!(collector.close().is_none());
}

#[test]
fn speakers_carry_talk_time_and_centroids_only_when_asked() {
    for centroids in [false, true] {
        let collector = start(Fake::default());
        assert!(collector.feed("others", 0.0, audio(&[0.1, 0.1, 0.3]), texts(3)));
        assert!(collector.feed("others", 4.5, audio(&[0.3, 0.1]), texts(2)));
        let result = collector.close().unwrap().wait_with(centroids);
        assert_eq!(result.status, Status::Ok);
        let stats: Vec<_> = result
            .speakers
            .iter()
            .map(|s| (s.source.as_str(), s.cluster, s.seconds))
            .collect();
        assert_eq!(stats, [("others", 0, 4.5), ("others", 1, 3.0)]);
        for stat in &result.speakers {
            match (&stat.centroid, centroids) {
                (None, false) => {}
                (Some(c), true) => {
                    let norm: f32 = c.iter().map(|v| v * v).sum::<f32>().sqrt();
                    assert!((norm - 1.0).abs() < 1e-6, "unit centroid");
                    assert_eq!(c.iter().filter(|&&v| v > 0.0).count(), 1);
                }
                other => panic!("centroid computed only when asked: {other:?}"),
            }
        }
    }
    let collector = start(Fake::default());
    assert!(collector.feed("others", 0.0, audio(&[0.1, 0.1]), texts(2)));
    let result = collector.close().unwrap().wait_with(true);
    assert_eq!(result.status, Status::Channel);
    assert!(result.speakers.is_empty(), "no speakers without labels");
}

#[test]
fn short_turns_are_not_embedded_and_text_falls_back_to_none() {
    let collector = start(Fake::default());
    // 0.5 s turn at the end is never embedded; its text is > 0.5 s from any embedded turn.
    let mut samples = audio(&[0.1, 0.3]);
    samples.extend(std::iter::repeat_n(0.5, SAMPLE_RATE as usize / 2));
    let timed = vec![
        TimedText {
            start: 0.0,
            end: 1.5,
            text: "a".into(),
        },
        TimedText {
            start: 1.5,
            end: 3.0,
            text: "b".into(),
        },
        TimedText {
            start: 3.6,
            end: 3.7,
            text: "c".into(),
        },
    ];
    collector.feed("others", 0.0, samples, timed);
    let result = collector.close().unwrap().wait();
    assert_eq!(
        result.chunks[0].runs,
        [
            run(0.0, 1.5, "a", Some(0)),
            run(1.5, 3.0, "b", Some(1)),
            run(3.6, 3.7, "c", None)
        ]
    );
}

#[test]
fn a_short_turn_keeps_its_own_text_instead_of_lending_it_to_a_long_neighbour() {
    // Turns: [0, 1.5) speaker A, [1.5, 3) speaker B, then a 0.5 s turn [3, 3.5)
    // that is too short to embed. Its text "c" touches B (gap 0) and fully
    // overlaps the short turn: overlap wins, so it stays unlabelled rather
    // than being attributed to B as it was when short turns were discarded.
    let collector = start(Fake::default());
    let mut samples = audio(&[0.1, 0.3]);
    samples.extend(std::iter::repeat_n(0.5, SAMPLE_RATE as usize / 2));
    let timed = vec![
        TimedText {
            start: 0.0,
            end: 1.5,
            text: "a".into(),
        },
        TimedText {
            start: 1.5,
            end: 3.0,
            text: "b".into(),
        },
        TimedText {
            start: 3.0,
            end: 3.5,
            text: "c".into(),
        },
    ];
    collector.feed("others", 0.0, samples, timed);
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Ok);
    assert_eq!(
        result.chunks[0].runs,
        [
            run(0.0, 1.5, "a", Some(0)),
            run(1.5, 3.0, "b", Some(1)),
            run(3.0, 3.5, "c", None)
        ]
    );
}

#[test]
fn unembedded_turns_get_keys_apart_from_clusters_and_are_never_clustered() {
    // Two short turns of the same backend speaker as a long one (bucket 1):
    // they must not join cluster 0, and text spanning both stays unlabelled.
    let collector = start(Fake::default());
    let mut samples = audio(&[0.1, 0.3]);
    samples.extend(std::iter::repeat_n(0.1, SAMPLE_RATE as usize / 2));
    let timed = vec![
        TimedText {
            start: 0.0,
            end: 1.5,
            text: "a".into(),
        },
        TimedText {
            start: 1.5,
            end: 3.0,
            text: "b".into(),
        },
        TimedText {
            start: 3.1,
            end: 3.4,
            text: "c".into(),
        },
    ];
    collector.feed("others", 0.0, samples, timed);
    let result = collector.close().unwrap().wait();
    assert_eq!(
        result.chunks[0].runs,
        [
            run(0.0, 1.5, "a", Some(0)),
            run(1.5, 3.0, "b", Some(1)),
            run(3.1, 3.4, "c", None)
        ]
    );
}

/// Returns fixed turns; embeds by amplitude bucket like `Fake`.
struct Scripted(Vec<SpeakerTurn>);
impl SpeakerBackend for Scripted {
    fn load(&mut self) -> Result<()> {
        Ok(())
    }
    fn diarize(&mut self, _: &[f32]) -> Result<Vec<SpeakerTurn>> {
        Ok(self.0.clone())
    }
    fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let mut v = vec![0.0; 8];
        v[bucket(audio[0])] = 1.0;
        Ok(v)
    }
}

#[test]
fn short_turns_of_one_local_speaker_sum_their_overlap_like_align_py() {
    let turn = |start, end, local_speaker| SpeakerTurn {
        start,
        end,
        local_speaker,
    };
    // Embedded: A [0, 1.5) and B [5, 6.5). Local speaker 7 has two 0.6 s turns,
    // each too short to embed. Text "x" [0.9, 3.0] overlaps A by 0.6 and each
    // short turn by 0.6: separately they only tie A (and A starts first), but
    // summed (1.2) they win, so "x" goes to the unembedded speaker -> None.
    let turns = vec![
        turn(0.0, 1.5, 0),
        turn(1.5, 2.1, 7),
        turn(2.4, 3.0, 7),
        turn(5.0, 6.5, 1),
    ];
    let level = |t: f64| if (5.0..6.5).contains(&t) { 0.3 } else { 0.1 };
    let samples: Vec<f32> = (0..7 * SAMPLE_RATE as usize)
        .map(|i| level(i as f64 / SAMPLE_RATE as f64))
        .collect();
    let timed = vec![
        TimedText {
            start: 0.0,
            end: 0.9,
            text: "a".into(),
        },
        TimedText {
            start: 0.9,
            end: 3.0,
            text: "x".into(),
        },
        TimedText {
            start: 5.0,
            end: 6.5,
            text: "z".into(),
        },
    ];
    let collector = Collector::start(Box::new(Scripted(turns)), Options::default()).unwrap();
    collector.feed("others", 0.0, samples, timed);
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Ok);
    assert_eq!(
        result.chunks[0].runs,
        [
            run(0.0, 0.9, "a", Some(0)),
            run(0.9, 3.0, "x", None),
            run(5.0, 6.5, "z", Some(1))
        ]
    );
}

#[test]
fn single_cluster_or_nothing_fed_keeps_channel() {
    let collector = start(Fake::default());
    collector.feed("others", 0.0, audio(&[0.1, 0.1]), texts(2));
    assert_eq!(collector.close().unwrap().wait().status, Status::Channel);
    let collector = start(Fake::default());
    collector.skip("others");
    assert_eq!(collector.close().unwrap().wait().status, Status::Channel);
}

#[test]
fn full_queue_drops_without_blocking_and_more_than_half_dropped_is_channel() {
    let (release, gate) = mpsc::channel();
    let chunks = Arc::new(AtomicUsize::new(0));
    let collector = start(Fake {
        gate: Some(Arc::new(Mutex::new(gate))),
        chunks: chunks.clone(),
        ..Fake::default()
    });
    let mut accepted = 0;
    for i in 0..QUEUE_CHUNKS + 4 {
        if collector.feed("others", i as f64 * 3.0, audio(&[0.1, 0.3]), texts(2)) {
            accepted += 1;
        }
    }
    // The worker holds at most one chunk outside the queue.
    assert!(accepted <= QUEUE_CHUNKS + 1, "{accepted}");
    assert!(accepted < QUEUE_CHUNKS + 4);
    let pending = collector.close().unwrap();
    for _ in 0..accepted {
        release.send(()).unwrap();
    }
    let result = pending.wait();
    assert_eq!(chunks.load(Ordering::SeqCst), accepted);
    // 8 offered, 3 or 4 dropped: at most half, so labels survive.
    assert_eq!(result.status, Status::Ok);

    let collector = start(Fake::default());
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    collector.skip("others");
    collector.skip("others");
    assert_eq!(collector.close().unwrap().wait().status, Status::Channel);
}

#[test]
fn three_consecutive_errors_disable_the_collector() {
    let chunks = Arc::new(AtomicUsize::new(0));
    let collector = start(Fake {
        fail_diarize: true,
        chunks: chunks.clone(),
        ..Fake::default()
    });
    for i in 0..3 {
        collector.feed("others", i as f64, audio(&[0.1, 0.3]), texts(2));
    }
    // Wait for the worker to see the third failure.
    while chunks.load(Ordering::SeqCst) < 3 {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(20));
    assert!(!collector.feed("others", 9.0, audio(&[0.1, 0.3]), texts(2)));
    assert_eq!(collector.close().unwrap().wait().status, Status::Channel);
    assert_eq!(chunks.load(Ordering::SeqCst), 3);
}

#[test]
fn panic_and_load_failure_are_contained() {
    let collector = start(Fake {
        panic_at: Some(1),
        ..Fake::default()
    });
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    assert_eq!(collector.close().unwrap().wait().status, Status::Failed);
    let collector = start(Fake {
        fail_load: true,
        ..Fake::default()
    });
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    assert_eq!(collector.close().unwrap().wait().status, Status::Failed);
}

#[test]
fn a_panic_after_good_chunks_falls_back_to_channel_for_the_whole_session() {
    let chunks = Arc::new(AtomicUsize::new(0));
    let collector = start(Fake {
        panic_at: Some(3),
        chunks: chunks.clone(),
        ..Fake::default()
    });
    // Two chunks that would relabel on their own (two speakers, nothing dropped yet).
    for i in 0..3 {
        assert!(collector.feed("others", i as f64 * 3.0, audio(&[0.1, 0.3]), texts(2)));
    }
    while chunks.load(Ordering::SeqCst) < 3 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Failed);
    assert!(result.chunks.is_empty());
}

#[test]
fn a_chunk_without_usable_turns_stays_a_channel_chunk() {
    let collector = start(Fake::default());
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    // 0.5 s: one turn below MIN_TURN_SECONDS, so nothing is embedded.
    let short = vec![0.5; SAMPLE_RATE as usize / 2];
    let timed = vec![TimedText {
        start: 0.0,
        end: 0.5,
        text: "short".into(),
    }];
    assert!(collector.feed("others", 3.0, short, timed));
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Ok);
    assert_eq!(
        result.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
        [0.0]
    );
}

#[test]
fn clustering_waits_for_wait_and_a_dropped_handle_cancels() {
    let (dropped_tx, dropped) = mpsc::channel();
    let chunks = Arc::new(AtomicUsize::new(0));
    let collector = start(Fake {
        _dropped: Some(DropSignal(dropped_tx)),
        chunks: chunks.clone(),
        ..Fake::default()
    });
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    let pending = collector.close().unwrap();
    while chunks.load(Ordering::SeqCst) < 1 {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Extraction is done, but the worker holds until `wait` (it would drop the
    // backend right before clustering).
    assert!(dropped.recv_timeout(Duration::from_millis(200)).is_err());
    let result = pending.wait();
    assert_eq!(result.status, Status::Ok);
    dropped.recv_timeout(Duration::from_secs(5)).unwrap();

    let (dropped_tx, dropped) = mpsc::channel();
    let collector = start(Fake {
        _dropped: Some(DropSignal(dropped_tx)),
        ..Fake::default()
    });
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    drop(collector.close().unwrap());
    // Cancelled: the worker exits without clustering.
    dropped.recv_timeout(Duration::from_secs(5)).unwrap();

    // Dropping the collector without ever closing it cancels as well.
    let (dropped_tx, dropped) = mpsc::channel();
    drop(start(Fake {
        _dropped: Some(DropSignal(dropped_tx)),
        ..Fake::default()
    }));
    dropped.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn finalize_times_out_without_joining_the_worker() {
    let (_release, gate) = mpsc::channel::<()>();
    let collector = Collector::start(
        Box::new(Fake {
            gate: Some(Arc::new(Mutex::new(gate))),
            ..Fake::default()
        }),
        Options {
            finalize_timeout: Duration::from_millis(50),
            ..Options::default()
        },
    )
    .unwrap();
    collector.feed("others", 0.0, audio(&[0.1, 0.3]), texts(2));
    let started = std::time::Instant::now();
    let result = collector.close().unwrap().wait();
    assert_eq!(result.status, Status::Timeout);
    assert!(result.chunks.is_empty());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn max_speakers_maps_auto_and_caps() {
    assert_eq!(max_speakers(0), MAX_SPEAKERS_CAP);
    assert_eq!(max_speakers(-3), MAX_SPEAKERS_CAP);
    assert_eq!(max_speakers(3), 3);
    assert_eq!(max_speakers(10), 10);
    assert_eq!(max_speakers(99), MAX_SPEAKERS_CAP);
    let collector = Collector::start(
        Box::new(Fake::default()),
        Options {
            max_speakers: 1,
            ..Options::default()
        },
    )
    .unwrap();
    collector.feed("others", 0.0, audio(&[0.1, 0.3, 0.5]), texts(3));
    assert_eq!(collector.close().unwrap().wait().status, Status::Channel);
}
