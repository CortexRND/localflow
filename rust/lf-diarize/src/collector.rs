//! Per-meeting speaker-label collector (design §5.3, §6).
//!
//! `feed` never blocks: chunks go to one worker thread through a bounded queue
//! and a full queue drops the chunk (it keeps its channel label). The worker
//! extracts turns and embeddings, then drops the audio. On `close` the worker
//! finishes extracting the queued chunks; only `PendingLabels::wait` lets it
//! cluster each source once and align the timed texts, and dropping that
//! handle cancels. Audio and embeddings stay in RAM and are never logged.

use crate::{
    align::{assign_speakers, merge_runs, SpeakerRun},
    cluster::cluster,
    SpeakerBackend, SpeakerTurn, TimedText, SAMPLE_RATE,
};
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Queued chunks (≈ 8 MB of 30 s 16 kHz audio).
pub const QUEUE_CHUNKS: usize = 4;
/// Only turns at least this long are embedded.
pub const MIN_TURN_SECONDS: f64 = 1.0;
/// Consecutive backend errors that disable the collector for the meeting.
pub const MAX_CONSECUTIVE_FAILURES: usize = 3;
pub const FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);
/// `max_speakers = 0` (auto) and anything larger mean this many.
pub const MAX_SPEAKERS_CAP: usize = 10;
/// Provisional cosine-distance threshold (sherpa-onnx's own clustering
/// default) until the B10 evaluation pins one per embedder.
pub const CLUSTER_THRESHOLD: f64 = 0.5;

/// `diarization_max_speakers` as a cluster cap: 0 (auto), negative or above the
/// hard cap all mean [`MAX_SPEAKERS_CAP`].
pub fn max_speakers(configured: i64) -> usize {
    match usize::try_from(configured) {
        Ok(n @ 1..=MAX_SPEAKERS_CAP) => n,
        _ => MAX_SPEAKERS_CAP,
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub threshold: f64,
    pub max_speakers: usize,
    pub finalize_timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            threshold: CLUSTER_THRESHOLD,
            max_speakers: MAX_SPEAKERS_CAP,
            finalize_timeout: FINALIZE_TIMEOUT,
        }
    }
}

/// Outcome names match the `/meeting/stop` `diarization` enum (§5.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// At least one source has ≥2 speakers; `chunks` holds its relabelled chunks.
    Ok,
    /// Nothing to relabel (≤1 cluster, too many dropped chunks, no timed chunks).
    Channel,
    Failed,
    Timeout,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Channel => "channel",
            Status::Failed => "failed",
            Status::Timeout => "timeout",
        }
    }
}

/// One chunk's runs; `speaker` is the source-local cluster, `None` keeps the channel label.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelledChunk {
    pub source: String,
    pub offset: f64,
    pub runs: Vec<SpeakerRun<Option<usize>>>,
}

/// One clustered speaker of a source: talk time and, only when asked for
/// (speaker recognition on), its unit centroid.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterStat {
    pub source: String,
    pub cluster: usize,
    /// Summed duration of the cluster's embedded turns. (Python also counts
    /// turns it labels by fallback; R3 leaves those unlabelled.)
    pub seconds: f64,
    /// L2-normalised mean of the cluster's unit vectors.
    pub centroid: Option<Vec<f32>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finalized {
    pub status: Status,
    pub chunks: Vec<LabelledChunk>,
    /// Per clustered speaker of the relabelled sources (empty unless `Ok`).
    pub speakers: Vec<ClusterStat>,
}
impl Finalized {
    fn empty(status: Status) -> Self {
        Self {
            status,
            chunks: Vec::new(),
            speakers: Vec::new(),
        }
    }
}

struct Job {
    source: String,
    offset: f64,
    audio: Vec<f32>,
    timed: Vec<TimedText>,
}
struct Chunk {
    source: String,
    offset: f64,
    timed: Vec<TimedText>,
    /// Every valid turn; embedded ones carry their L2-normalised vector.
    turns: Vec<(SpeakerTurn, Option<Vec<f64>>)>,
}
#[derive(Clone, Copy, Default)]
struct Counts {
    offered: usize,
    dropped: usize,
}
#[derive(Default)]
struct Shared {
    counts: Mutex<BTreeMap<String, Counts>>,
    disabled: AtomicBool,
    load_failed: AtomicBool,
    panicked: AtomicBool,
}
impl Shared {
    fn count(&self, source: &str, dropped: bool) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = counts.entry(source.to_string()).or_default();
        entry.offered += 1;
        entry.dropped += usize::from(dropped);
    }
    fn drop_offered(&self, source: &str) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        counts.entry(source.to_string()).or_default().dropped += 1;
    }
}

pub struct Collector {
    tx: Mutex<Option<SyncSender<Job>>>,
    done: Mutex<Option<(Sender<bool>, Receiver<Finalized>)>>,
    shared: Arc<Shared>,
    timeout: Duration,
}

/// Handle to a closed collector's result. Clustering starts only in
/// [`wait`](Self::wait) (after the caller's channel checkpoint); dropping the
/// handle cancels it, and the worker exits without clustering.
pub struct PendingLabels {
    go: Sender<bool>,
    done: Receiver<Finalized>,
    timeout: Duration,
}
impl PendingLabels {
    /// Starts clustering and blocks at most the finalize timeout.
    pub fn wait(self) -> Finalized {
        self.wait_with(false)
    }
    /// [`wait`](Self::wait), also computing each speaker's centroid when
    /// `centroids` (speaker recognition on); otherwise none is ever computed.
    pub fn wait_with(self, centroids: bool) -> Finalized {
        let started = Instant::now();
        // A worker that already exited (it panicked outside extraction) reads as failed below.
        let _ = self.go.send(centroids);
        let result = match self.done.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Finalized::empty(Status::Timeout),
            Err(RecvTimeoutError::Disconnected) => Finalized::empty(Status::Failed),
        };
        log::info!(
            "speaker labels {}: {} chunks relabelled, waited {} ms",
            result.status.as_str(),
            result.chunks.len(),
            started.elapsed().as_millis()
        );
        result
    }
}

impl Collector {
    /// Starts the worker; `backend.load()` runs there, so meeting start never waits for it.
    pub fn start(backend: Box<dyn SpeakerBackend>, options: Options) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (done_tx, done) = mpsc::channel();
        let (go, go_rx) = mpsc::channel::<bool>();
        let shared = Arc::<Shared>::default();
        let timeout = options.finalize_timeout;
        let worker_shared = shared.clone();
        thread::Builder::new()
            .name("localflow-speaker-labels".into())
            .spawn(move || {
                let (chunks, backend) = collect(backend, rx, &worker_shared);
                // Queued chunks are extracted; clustering waits for `PendingLabels::wait`.
                let Ok(centroids) = go_rx.recv() else {
                    log::debug!("speaker labels cancelled before clustering");
                    return;
                };
                drop(backend);
                let result = if worker_shared.load_failed.load(Ordering::SeqCst)
                    || worker_shared.panicked.load(Ordering::SeqCst)
                {
                    // A backend panic voids the whole session, not just its chunk.
                    Finalized::empty(Status::Failed)
                } else {
                    let counts = worker_shared
                        .counts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    catch_unwind(AssertUnwindSafe(|| {
                        finalize(chunks, &counts, &options, centroids)
                    }))
                    .unwrap_or_else(|_| Finalized::empty(Status::Failed))
                };
                let _ = done_tx.send(result);
            })?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            done: Mutex::new(Some((go, done))),
            shared,
            timeout,
        })
    }

    /// Hands a transcribed chunk to the worker without blocking; the audio is
    /// moved and freed after extraction. Returns false when the chunk was dropped.
    pub fn feed(&self, source: &str, offset: f64, audio: Vec<f32>, timed: Vec<TimedText>) -> bool {
        let accepted = !self.shared.disabled.load(Ordering::SeqCst)
            && match self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                Some(tx) => match tx.try_send(Job {
                    source: source.to_string(),
                    offset,
                    audio,
                    timed,
                }) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                },
                None => false,
            };
        self.shared.count(source, !accepted);
        accepted
    }

    /// Records a transcribed chunk without real timings; it keeps its channel label.
    pub fn skip(&self, source: &str) {
        self.shared.count(source, true);
    }

    /// Closes the queue; the worker finishes extracting queued chunks but does
    /// not cluster until [`PendingLabels::wait`]. `None` after the first call.
    pub fn close(&self) -> Option<PendingLabels> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let (go, done) = self.done.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        Some(PendingLabels {
            go,
            done,
            timeout: self.timeout,
        })
    }
}

fn collect(
    mut backend: Box<dyn SpeakerBackend>,
    rx: Receiver<Job>,
    shared: &Shared,
) -> (Vec<Chunk>, Box<dyn SpeakerBackend>) {
    let loaded = catch_unwind(AssertUnwindSafe(|| backend.load()));
    if !matches!(loaded, Ok(Ok(()))) {
        log::warn!("speaker labels unavailable: backend failed to load");
        shared.load_failed.store(true, Ordering::SeqCst);
        shared.disabled.store(true, Ordering::SeqCst);
    }
    let mut chunks = Vec::new();
    let mut failures = 0;
    for job in rx {
        if shared.disabled.load(Ordering::SeqCst) {
            shared.drop_offered(&job.source);
            continue;
        }
        let started = Instant::now();
        let seconds = job.audio.len() as f64 / SAMPLE_RATE as f64;
        match catch_unwind(AssertUnwindSafe(|| extract(&mut *backend, &job.audio))) {
            Ok(Ok(turns)) => {
                failures = 0;
                log::debug!(
                    "speaker turns ({}): {} ({} embedded) from {seconds:.1}s in {} ms",
                    job.source,
                    turns.len(),
                    turns.iter().filter(|(_, v)| v.is_some()).count(),
                    started.elapsed().as_millis()
                );
                chunks.push(Chunk {
                    source: job.source,
                    offset: job.offset,
                    timed: job.timed,
                    turns,
                });
            }
            Ok(Err(_)) => {
                failures += 1;
                log::warn!(
                    "speaker extraction failed ({}); chunk keeps channel label",
                    job.source
                );
                shared.drop_offered(&job.source);
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    log::warn!("speaker labels disabled after {failures} consecutive failures");
                    shared.disabled.store(true, Ordering::SeqCst);
                }
            }
            Err(_) => {
                log::warn!("speaker extraction panicked; speaker labels disabled");
                shared.drop_offered(&job.source);
                shared.panicked.store(true, Ordering::SeqCst);
                shared.disabled.store(true, Ordering::SeqCst);
            }
        }
        // `job.audio` is dropped here: nothing of the waveform is kept.
    }
    (chunks, backend)
}

fn extract(
    backend: &mut dyn SpeakerBackend,
    audio: &[f32],
) -> anyhow::Result<Vec<(SpeakerTurn, Option<Vec<f64>>)>> {
    let rate = SAMPLE_RATE as f64;
    let duration = audio.len() as f64 / rate;
    let mut turns = Vec::new();
    for turn in backend.diarize(audio)? {
        // Every finite, non-empty turn inside the audio takes part in alignment.
        if !turn.start.is_finite() || !turn.end.is_finite() {
            continue;
        }
        let turn = SpeakerTurn {
            start: turn.start.max(0.0),
            end: turn.end.min(duration),
            ..turn
        };
        if turn.start >= turn.end {
            continue;
        }
        // Only turns of at least MIN_TURN_SECONDS are embedded (and clustered).
        let from = ((turn.start * rate) as usize).min(audio.len());
        let to = ((turn.end * rate) as usize).min(audio.len());
        if (to.saturating_sub(from) as f64) < MIN_TURN_SECONDS * rate {
            turns.push((turn, None));
            continue;
        }
        let vector = backend.embed(&audio[from..to])?;
        let vector: Vec<f64> = vector.iter().map(|&v| v as f64).collect();
        let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
        let vector = (!vector.is_empty() && norm.is_finite() && norm > 0.0)
            .then(|| vector.iter().map(|v| v / norm).collect());
        turns.push((turn, vector));
    }
    Ok(turns)
}

fn finalize(
    chunks: Vec<Chunk>,
    counts: &BTreeMap<String, Counts>,
    options: &Options,
    centroids: bool,
) -> Finalized {
    let mut by_source: BTreeMap<&str, Vec<&Chunk>> = BTreeMap::new();
    for chunk in &chunks {
        by_source.entry(&chunk.source).or_default().push(chunk);
    }
    let mut labelled = Vec::new();
    let mut speakers = Vec::new();
    for (source, chunks) in by_source {
        let count = counts.get(source).copied().unwrap_or_default();
        if count.dropped * 2 > count.offered {
            continue;
        }
        // Turns without a vector are never clustered.
        let vectors: Vec<&[f64]> = chunks
            .iter()
            .flat_map(|c| c.turns.iter().filter_map(|(_, v)| v.as_deref()))
            .collect();
        let labels = match cluster(&vectors, options.threshold, Some(options.max_speakers)) {
            Ok(labels) => labels,
            Err(_) => return Finalized::empty(Status::Failed),
        };
        // Q4: a single remote speaker keeps "Others".
        let clusters = labels.iter().max().map_or(0, |top| top + 1);
        if clusters < 2 {
            continue;
        }
        // Talk time and (only when asked) the unit mean vector per cluster.
        let mut seconds = vec![0.0; clusters];
        let mut sums: Vec<Vec<f64>> = vec![Vec::new(); clusters];
        let embedded = chunks
            .iter()
            .flat_map(|c| c.turns.iter())
            .filter_map(|(turn, v)| Some((turn, v.as_deref()?)));
        for ((turn, vector), &label) in embedded.zip(&labels) {
            seconds[label] += turn.end - turn.start;
            if centroids {
                let sum = &mut sums[label];
                sum.resize(vector.len(), 0.0);
                for (s, v) in sum.iter_mut().zip(vector) {
                    *s += *v;
                }
            }
        }
        for (cluster, (seconds, sum)) in seconds.into_iter().zip(sums).enumerate() {
            let norm = sum.iter().map(|v| v * v).sum::<f64>().sqrt();
            let centroid = (centroids && norm.is_finite() && norm > 0.0)
                .then(|| sum.iter().map(|v| (v / norm) as f32).collect());
            speakers.push(ClusterStat {
                source: source.to_string(),
                cluster,
                seconds,
                centroid,
            });
        }
        let mut labels = labels.into_iter();
        for chunk in chunks {
            if chunk.turns.iter().all(|(_, v)| v.is_none()) {
                continue; // no embedded turn: a plain channel chunk
            }
            // Embedded turns carry their cluster. Unembedded turns get one key
            // per chunk-local speaker, at or above `clusters`: like align.py,
            // that speaker's turns sum their overlap, but the key never merges
            // with a real cluster (no linkage to embedded turns is inferred).
            let mut untracked: Vec<usize> = Vec::new();
            let turns: Vec<SpeakerTurn> = chunk
                .turns
                .iter()
                .map(|(turn, vector)| SpeakerTurn {
                    local_speaker: match vector {
                        Some(_) => labels.next().expect("one label per vector"),
                        None => {
                            let at = untracked
                                .iter()
                                .position(|&s| s == turn.local_speaker)
                                .unwrap_or_else(|| {
                                    untracked.push(turn.local_speaker);
                                    untracked.len() - 1
                                });
                            clusters + at
                        }
                    },
                    ..*turn
                })
                .collect();
            let assigned: Vec<_> = assign_speakers(&chunk.timed, &turns)
                .into_iter()
                .map(|(text, speaker)| (text, speaker.filter(|&s| s < clusters)))
                .collect();
            labelled.push(LabelledChunk {
                source: chunk.source.clone(),
                offset: chunk.offset,
                runs: merge_runs(&assigned, chunk.offset),
            });
        }
    }
    if labelled.is_empty() {
        return Finalized::empty(Status::Channel);
    }
    Finalized {
        status: Status::Ok,
        chunks: labelled,
        speakers,
    }
}
