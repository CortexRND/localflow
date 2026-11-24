//! The appearance choice (System / Light / Dark) of the desktop window.
//!
//! Stored on its own, next to this app's window lock:
//! `<instance dir>/appearance.json` (real data: `<localflow dir>/desktop/`;
//! sandbox: inside the sandbox directory). It is never written to
//! `config.toml` and never goes through `Services::apply`, so dictation and
//! provider settings can't be affected by it. A missing, unreadable or
//! unknown file reads as System; the next save repairs it.
//!
//! Choices are saved off the UI thread by one serialized writer ([`queue`]):
//! writes run in the order they were chosen, so the last choice wins, and a
//! stalled disk never holds up a repaint.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::Value;

use crate::controllers::instance::instance_dir;
use crate::data::DataRoot;

/// File name inside the instance directory.
pub const FILE_NAME: &str = "appearance.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppearanceMode {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// In the order the Settings control shows them.
    pub const ALL: [AppearanceMode; 3] = [
        AppearanceMode::System,
        AppearanceMode::Light,
        AppearanceMode::Dark,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AppearanceMode::System => "System",
            AppearanceMode::Light => "Light",
            AppearanceMode::Dark => "Dark",
        }
    }

    /// The stored value.
    pub fn key(self) -> &'static str {
        match self {
            AppearanceMode::System => "system",
            AppearanceMode::Light => "light",
            AppearanceMode::Dark => "dark",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.key() == key)
    }

    /// Position in [`Self::ALL`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
}

/// Where the choice is stored for `root`.
pub fn path(root: &DataRoot) -> PathBuf {
    instance_dir(root).join(FILE_NAME)
}

/// The stored choice; System when the file is missing, unreadable or holds
/// something unknown (never an error: the window just follows the OS).
pub fn load(root: &DataRoot) -> AppearanceMode {
    std::fs::read_to_string(path(root))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("mode")
                .and_then(Value::as_str)
                .and_then(AppearanceMode::from_key)
        })
        .unwrap_or_default()
}

/// Store the choice: written to a temporary sibling, then renamed over the
/// file, so a reader never sees a half-written file.
pub fn save(root: &DataRoot, mode: AppearanceMode) -> std::io::Result<()> {
    let target = path(root);
    let dir = target
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| instance_dir(root));
    std::fs::create_dir_all(&dir)?;
    // Unique per write, so two writes can never share a temporary file.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{FILE_NAME}.{}.{seq}.tmp", std::process::id()));
    let body = format!("{{\n  \"mode\": \"{}\"\n}}\n", mode.key());
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(body.as_bytes())?;
        file.sync_all()
    });
    let result = written.and_then(|()| std::fs::rename(&temp, &target));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The message shown on the Appearance row for a failed save.
fn failure(err: &str) -> String {
    format!("Couldn't save the appearance ({err}). It applies until LocalFlow closes.")
}

/// A finished save, sent back to whoever queued it.
#[derive(Debug)]
pub struct Saved {
    pub generation: u64,
    pub mode: AppearanceMode,
    pub result: Result<(), String>,
}

struct Job {
    generation: u64,
    root: DataRoot,
    mode: AppearanceMode,
    /// Held until the write is on disk (a shutdown pending-write guard).
    guard: Box<dyn Send>,
    /// Holds one message; awaited (not waited on by a thread) by the caller.
    done: async_channel::Sender<Saved>,
}

#[derive(Debug, Default)]
struct Status {
    /// Generation of the most recently queued choice.
    latest: u64,
    /// Highest generation whose save has ended (FIFO: all before it too).
    finished: u64,
    /// Why the latest choice isn't stored; only the latest choice's outcome
    /// is ever recorded, so an older failure can't show over a newer save.
    error: Option<String>,
}

impl Status {
    fn finish(&mut self, generation: u64, result: &Result<(), String>) {
        self.finished = self.finished.max(generation);
        if generation == self.latest {
            self.error = result.as_ref().err().map(|e| failure(e));
        }
    }
}

type Start = fn(Arc<Mutex<Status>>, Receiver<Job>) -> std::io::Result<()>;

/// The serialized appearance writer: one worker thread, started on first
/// use (and again if it ever stopped), saving queued choices in order.
pub struct SaveQueue {
    jobs: Mutex<Option<Sender<Job>>>,
    status: Arc<Mutex<Status>>,
    start: Start,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn start_worker(status: Arc<Mutex<Status>>, jobs: Receiver<Job>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("appearance-save".into())
        .spawn(move || {
            // Ends once every sender is gone and the queue is drained.
            for job in jobs {
                write(&status, job);
            }
        })
        .map(drop)
}

/// Store one queued choice, record it, report it, then release its guard.
fn write(status: &Mutex<Status>, job: Job) {
    let Job {
        generation,
        root,
        mode,
        guard,
        done,
    } = job;
    let result = save(&root, mode).map_err(|e| e.to_string());
    lock(status).finish(generation, &result);
    // Never blocks: the channel holds exactly this one message.
    let _ = done.try_send(Saved {
        generation,
        mode,
        result,
    });
    drop(guard);
}

impl Default for SaveQueue {
    fn default() -> Self {
        Self::with_start(start_worker)
    }
}

impl SaveQueue {
    fn with_start(start: Start) -> Self {
        SaveQueue {
            jobs: Mutex::new(None),
            status: Arc::default(),
            start,
        }
    }

    /// Queue `mode` to be stored for `root`, holding `guard` until it is on
    /// disk. Never blocks on the disk. An error (the writer couldn't start
    /// or stopped) is also recorded as the latest outcome.
    pub fn submit(
        &self,
        root: &DataRoot,
        mode: AppearanceMode,
        guard: Box<dyn Send>,
    ) -> Result<async_channel::Receiver<Saved>, String> {
        // Held while numbering and sending, so queue order = generation order.
        let mut jobs = lock(&self.jobs);
        let generation = {
            let mut status = lock(&self.status);
            status.latest += 1;
            // An earlier failure no longer describes the latest choice.
            status.error = None;
            status.latest
        };
        let (done, receiver) = async_channel::bounded(1);
        let job = Job {
            generation,
            root: root.clone(),
            mode,
            guard,
            done,
        };
        let job = match jobs.as_ref() {
            Some(sender) => match sender.send(job) {
                Ok(()) => return Ok(receiver),
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        *jobs = None;
        let (sender, worker) = mpsc::channel();
        let err = match (self.start)(self.status.clone(), worker) {
            Ok(()) => match sender.send(job) {
                Ok(()) => {
                    *jobs = Some(sender);
                    return Ok(receiver);
                }
                Err(_) => "the appearance writer stopped".to_string(),
            },
            Err(e) => format!("couldn't start the appearance writer: {e}"),
        };
        lock(&self.status).finish(generation, &Err(err.clone()));
        Err(err)
    }

    /// Why the latest choice isn't stored, if it isn't.
    pub fn error(&self) -> Option<String> {
        lock(&self.status).error.clone()
    }

    /// Whether choosing `mode` while `selected` is shown needs a save: a
    /// different mode always does; the selected one only while a save is
    /// still outstanding or the latest one failed (a retry).
    pub fn needs_save(&self, mode: AppearanceMode, selected: AppearanceMode) -> bool {
        if mode != selected {
            return true;
        }
        let status = lock(&self.status);
        status.finished < status.latest || status.error.is_some()
    }
}

/// The app's one appearance writer: one ordered queue for the process.
pub fn queue() -> &'static SaveQueue {
    static QUEUE: OnceLock<SaveQueue> = OnceLock::new();
    QUEUE.get_or_init(SaveQueue::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ConfigFile;
    use serde_json::json;
    use std::time::Duration;

    /// Reports what the stored file reads as when its write's guard drops.
    struct Probe(
        DataRoot,
        AppearanceMode,
        Sender<(AppearanceMode, AppearanceMode)>,
    );
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.2.send((self.1, load(&self.0)));
        }
    }

    const WAIT: Duration = Duration::from_secs(10);

    /// A save's report, failing the test (not hanging) if it never comes.
    fn wait(receiver: &async_channel::Receiver<Saved>) -> Saved {
        let receiver = receiver.clone();
        let (sent, report) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sent.send(receiver.recv_blocking());
        });
        report
            .recv_timeout(WAIT)
            .expect("the save reported in time")
            .expect("the writer reported before ending")
    }

    /// A save that has not reported yet (never blocks).
    fn not_reported(receiver: &async_channel::Receiver<Saved>) -> bool {
        matches!(receiver.try_recv(), Err(async_channel::TryRecvError::Empty))
    }

    /// Each test's gate for [`gated`]; only one test uses it.
    static GATE: Mutex<Option<Receiver<()>>> = Mutex::new(None);

    /// The real writer, except each save waits for one `()` on [`GATE`]:
    /// a disk that stalls until the test releases it.
    fn gated(status: Arc<Mutex<Status>>, jobs: Receiver<Job>) -> std::io::Result<()> {
        let gate = lock(&GATE).take().expect("the gate is set");
        std::thread::spawn(move || {
            for job in jobs {
                let _ = gate.recv();
                write(&status, job);
            }
        });
        Ok(())
    }

    #[test]
    fn a_stalled_save_never_blocks_choosing_or_reading_and_the_latest_persists() {
        let (_dir, root) = root();
        let file = path(&root);
        let (release, gate) = mpsc::channel();
        *lock(&GATE) = Some(gate);
        let queue = SaveQueue::with_start(gated);
        let (probe, probes) = mpsc::channel();
        let guard = |mode| Box::new(Probe(root.clone(), mode, probe.clone()));

        // A first choice fails (a directory blocks the rename).
        std::fs::create_dir_all(&file).unwrap();
        let first = queue
            .submit(&root, AppearanceMode::Light, guard(AppearanceMode::Light))
            .unwrap();
        release.send(()).unwrap();
        assert!(wait(&first).result.is_err());
        assert_eq!(probes.recv_timeout(WAIT).unwrap().0, AppearanceMode::Light);
        assert!(queue.error().is_some());
        std::fs::remove_dir(&file).unwrap();

        // The disk now stalls: every choice still queues at once.
        let order = [AppearanceMode::System, AppearanceMode::Dark];
        let held: Vec<_> = order
            .into_iter()
            .map(|mode| queue.submit(&root, mode, guard(mode)).unwrap())
            .collect();
        assert_eq!(
            queue.error(),
            None,
            "the old failure isn't shown over newer choices"
        );
        assert!(
            queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark),
            "the selected choice is still outstanding"
        );
        assert!(held.iter().all(not_reported));
        assert!(
            matches!(probes.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "no pending write is released before it is on disk"
        );
        assert_eq!(load(&root), AppearanceMode::System, "nothing stored yet");

        // The disk recovers: both writes land in order, the last one wins.
        release.send(()).unwrap();
        release.send(()).unwrap();
        for (receiver, mode) in held.iter().zip(order) {
            let saved = wait(receiver);
            assert_eq!((saved.mode, saved.result), (mode, Ok(())));
            assert_eq!(probes.recv_timeout(WAIT).unwrap(), (mode, mode));
        }
        assert_eq!(load(&root), AppearanceMode::Dark);
        assert_eq!(queue.error(), None);
        assert!(!queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark));
    }

    fn temp_files(root: &DataRoot) -> Vec<String> {
        std::fs::read_dir(path(root).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(FILE_NAME) && n != FILE_NAME)
            .collect()
    }

    #[test]
    fn rapid_choices_are_saved_in_order_and_the_last_one_wins() {
        let (_dir, root) = root();
        let queue = SaveQueue::default();
        let (probe, probes) = mpsc::channel();
        let order = [
            AppearanceMode::Dark,
            AppearanceMode::Light,
            AppearanceMode::System,
            AppearanceMode::Dark,
        ];
        let receivers: Vec<_> = order
            .iter()
            .map(|&mode| {
                let guard = Box::new(Probe(root.clone(), mode, probe.clone()));
                queue.submit(&root, mode, guard).unwrap()
            })
            .collect();
        for (i, (receiver, mode)) in receivers.iter().zip(order).enumerate() {
            let saved = wait(receiver);
            assert_eq!(saved.generation, i as u64 + 1);
            assert_eq!(saved.mode, mode);
            assert_eq!(saved.result, Ok(()));
            // The guard is held until that write is on disk, and no later
            // write ran before it.
            assert_eq!(probes.recv_timeout(WAIT).unwrap(), (mode, mode));
        }
        assert_eq!(load(&root), AppearanceMode::Dark);
        assert_eq!(queue.error(), None);
        assert!(temp_files(&root).is_empty(), "{:?}", temp_files(&root));
        assert!(!queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark));
        assert!(queue.needs_save(AppearanceMode::Light, AppearanceMode::Dark));
    }

    #[test]
    fn a_failed_latest_save_stays_visible_and_the_same_choice_can_retry() {
        let (_dir, root) = root();
        let file = path(&root);
        std::fs::create_dir_all(&file).unwrap();
        let queue = SaveQueue::default();

        let saved = wait(
            &queue
                .submit(&root, AppearanceMode::Dark, Box::new(()))
                .unwrap(),
        );
        assert!(saved.result.is_err());
        let error = queue.error().expect("the failure is kept");
        assert!(
            error.starts_with("Couldn't save the appearance ("),
            "{error}"
        );
        assert!(
            queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark),
            "the selected choice can be retried after a failure"
        );

        std::fs::remove_dir(&file).unwrap();
        let saved = wait(
            &queue
                .submit(&root, AppearanceMode::Dark, Box::new(()))
                .unwrap(),
        );
        assert_eq!(saved.result, Ok(()));
        assert_eq!(queue.error(), None, "the retry clears the error");
        assert_eq!(load(&root), AppearanceMode::Dark);
        assert!(!queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark));
    }

    #[test]
    fn only_the_latest_outcome_is_recorded() {
        let mut status = Status {
            latest: 2,
            ..Status::default()
        };
        // An older write failing while a newer one is queued: not shown, and
        // the newer one is still outstanding.
        status.finish(1, &Err("disk".into()));
        assert_eq!(status.error, None);
        assert!(status.finished < status.latest);
        status.finish(2, &Err("full".into()));
        assert!(status.error.as_deref().unwrap().contains("full"));
        status.latest = 3;
        status.finish(3, &Ok(()));
        assert_eq!(status.error, None);
        assert_eq!(status.finished, 3);
    }

    #[test]
    fn a_writer_that_cannot_start_reports_and_releases_the_write() {
        fn refuse(_: Arc<Mutex<Status>>, _: Receiver<Job>) -> std::io::Result<()> {
            Err(std::io::Error::other("no threads"))
        }
        let (_dir, root) = root();
        let queue = SaveQueue::with_start(refuse);
        let (probe, probes) = mpsc::channel();
        let guard = Box::new(Probe(root.clone(), AppearanceMode::Dark, probe));
        let err = queue
            .submit(&root, AppearanceMode::Dark, guard)
            .unwrap_err();
        assert!(err.contains("no threads"), "{err}");
        assert!(queue.error().unwrap().contains("no threads"));
        assert_eq!(
            probes.recv_timeout(WAIT).unwrap(),
            (AppearanceMode::Dark, AppearanceMode::System),
            "the pending write is released, nothing was stored"
        );
        assert!(queue.needs_save(AppearanceMode::Dark, AppearanceMode::Dark));
    }

    #[test]
    fn a_stopped_writer_is_restarted_for_the_next_choice() {
        fn stop(_: Arc<Mutex<Status>>, jobs: Receiver<Job>) -> std::io::Result<()> {
            drop(jobs);
            Ok(())
        }
        let (_dir, root) = root();
        let mut queue = SaveQueue::with_start(stop);
        assert!(queue
            .submit(&root, AppearanceMode::Light, Box::new(()))
            .unwrap_err()
            .contains("stopped"));
        queue.start = start_worker;
        let saved = wait(
            &queue
                .submit(&root, AppearanceMode::Light, Box::new(()))
                .unwrap(),
        );
        assert_eq!(saved.generation, 2);
        assert_eq!(saved.result, Ok(()));
        assert_eq!(queue.error(), None);
        assert_eq!(load(&root), AppearanceMode::Light);
    }

    fn root() -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        (dir, root)
    }

    #[test]
    fn a_fresh_root_follows_the_system_and_every_choice_round_trips() {
        let (_dir, root) = root();
        assert!(!path(&root).exists());
        assert_eq!(load(&root), AppearanceMode::System);
        for mode in [
            AppearanceMode::Dark,
            AppearanceMode::Light,
            AppearanceMode::System,
        ] {
            save(&root, mode).unwrap();
            assert_eq!(load(&root), mode);
        }
        // No temporary file is left next to it.
        let names: Vec<_> = std::fs::read_dir(path(&root).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(FILE_NAME))
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()]);
    }

    #[test]
    fn garbage_or_unknown_values_read_as_system_and_a_save_repairs_them() {
        let (_dir, root) = root();
        let file = path(&root);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        for content in [
            "not json at all",
            "",
            "{\"mode\": \"sepia\"}",
            "{\"mode\": 3}",
            "[\"dark\"]",
            "{\"mode\": \"Dark\"}",
        ] {
            std::fs::write(&file, content).unwrap();
            assert_eq!(load(&root), AppearanceMode::System, "{content:?}");
        }
        save(&root, AppearanceMode::Dark).unwrap();
        assert_eq!(load(&root), AppearanceMode::Dark);
        save(&root, AppearanceMode::Light).unwrap();
        assert_eq!(load(&root), AppearanceMode::Light);
    }

    #[test]
    fn a_failed_save_returns_an_error_leaves_no_temp_file_and_reads_as_system() {
        let (_dir, root) = root();
        let file = path(&root);
        // A directory where the file belongs: the rename over it must fail.
        std::fs::create_dir_all(&file).unwrap();
        std::fs::write(file.join("keep"), "x").unwrap();

        assert!(save(&root, AppearanceMode::Dark).is_err());
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(FILE_NAME))
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()], "no temp file is left");
        assert!(file.is_dir(), "the blocking directory is untouched");
        assert!(file.join("keep").is_file());
        assert_eq!(load(&root), AppearanceMode::System);
    }

    #[test]
    fn appearance_and_config_are_stored_independently() {
        let (_dir, root) = root();
        let config = ConfigFile::new(root.clone());
        config
            .apply(&json!({"dictation": {"enabled": true}}))
            .unwrap();
        let before = std::fs::read(&root.config_path).unwrap();

        save(&root, AppearanceMode::Dark).unwrap();
        assert_eq!(
            std::fs::read(&root.config_path).unwrap(),
            before,
            "saving the appearance leaves config.toml byte-identical"
        );
        assert_ne!(path(&root), root.config_path);

        let saved = config
            .apply(&json!({"dictation": {"enabled": false}}))
            .unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert_eq!(
            load(&root),
            AppearanceMode::Dark,
            "a config update leaves the stored appearance unchanged"
        );
        let text = std::fs::read_to_string(&root.config_path).unwrap();
        assert!(!text.contains("appearance"), "{text}");
    }

    #[test]
    fn a_sandbox_keeps_its_appearance_inside_the_sandbox() {
        let (dir, root) = root();
        let file = path(&root);
        assert!(file.starts_with(dir.path()), "{}", file.display());
        save(&root, AppearanceMode::Light).unwrap();
        assert!(file.is_file());
        assert!(file.starts_with(root.sandbox_dir.as_ref().unwrap()));
    }

    #[test]
    fn keys_and_indices_are_stable() {
        for (i, mode) in AppearanceMode::ALL.into_iter().enumerate() {
            assert_eq!(AppearanceMode::from_key(mode.key()), Some(mode));
            assert_eq!(mode.index(), i);
        }
        assert_eq!(AppearanceMode::default(), AppearanceMode::System);
    }
}
