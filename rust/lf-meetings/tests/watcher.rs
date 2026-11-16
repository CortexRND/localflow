use anyhow::{anyhow, Result};
use lf_meetings::{
    watcher::{detect_platform_with, ProbeFactory},
    MeetingWatcher, MicUsageProbe,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

struct Probe {
    busy: Arc<AtomicBool>,
    samples: mpsc::Sender<()>,
    fail: Arc<AtomicBool>,
}
impl MicUsageProbe for Probe {
    fn mic_in_use(&self) -> Result<bool> {
        let failed = self.fail.load(Ordering::SeqCst);
        let _ = self.samples.send(());
        if failed {
            Err(anyhow!("probe error"))
        } else {
            Ok(self.busy.load(Ordering::SeqCst))
        }
    }
}

#[test]
fn watcher_stop_start_reuses_running_thread_then_recreates_after_exit() {
    struct BlockingProbe {
        entered: mpsc::Sender<()>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
        exited: mpsc::Sender<()>,
    }
    impl MicUsageProbe for BlockingProbe {
        fn mic_in_use(&self) -> Result<bool> {
            self.entered.send(())?;
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))?;
            Ok(false)
        }
    }
    impl Drop for BlockingProbe {
        fn drop(&mut self) {
            let _ = self.exited.send(());
        }
    }
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let (exited_tx, exited) = mpsc::channel();
    let factories = Arc::new(AtomicUsize::new(0));
    let count = factories.clone();
    let watcher = MeetingWatcher::new(Duration::ZERO, Duration::from_millis(5), None)
        .unwrap()
        .with_probe_factory(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(BlockingProbe {
                entered: entered_tx.clone(),
                release: release_rx.clone(),
                exited: exited_tx.clone(),
            }))
        }));
    watcher.start();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    watcher.start();
    watcher.stop();
    watcher.start();
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    watcher.stop();
    release.send(()).unwrap();
    exited.recv_timeout(Duration::from_secs(3)).unwrap();
    watcher.start();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(factories.load(Ordering::SeqCst), 2);
    watcher.stop();
    release.send(()).unwrap();
    exited.recv_timeout(Duration::from_secs(3)).unwrap();
}
#[test]
fn platform_detection_preserves_order_matching_modes_and_other() {
    let mut queried = Vec::new();
    assert_eq!(
        detect_platform_with(|flag, name| {
            queried.push((flag.to_owned(), name.to_owned()));
            name == "Microsoft Teams"
        }),
        "Teams"
    );
    assert_eq!(
        queried,
        [
            ("-x".into(), "zoom.us".into()),
            ("-f".into(), "Microsoft Teams".into())
        ]
    );
    assert_eq!(detect_platform_with(|_, _| true), "Zoom");
    assert_eq!(detect_platform_with(|_, _| false), "Other");
    for (expected, _, process) in lf_meetings::watcher::PLATFORM_PROCESSES {
        assert_eq!(detect_platform_with(|_, name| name == *process), *expected);
    }
}
#[test]
fn watcher_unavailable_records_error_and_can_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let watcher = MeetingWatcher::default().with_probe_factory(Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Err(anyhow!("no CoreAudio"))
    }));
    watcher.start();
    assert_eq!(
        watcher.status().error,
        "mic watcher unavailable: no CoreAudio"
    );
    watcher.start();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(MeetingWatcher::new(Duration::ZERO, Duration::ZERO, None).is_err());
}
#[test]
fn watcher_pause_resume_dismiss_callback_errors_probe_errors_and_lifecycle() {
    let busy = Arc::new(AtomicBool::new(true));
    let fail = Arc::new(AtomicBool::new(false));
    let factories = Arc::new(AtomicUsize::new(0));
    let (sample_tx, sample_rx) = mpsc::channel();
    let (detect_tx, detect_rx) = mpsc::channel();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let factory: ProbeFactory = {
        let busy = busy.clone();
        let fail = fail.clone();
        let factories = factories.clone();
        Arc::new(move || {
            factories.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Probe {
                busy: busy.clone(),
                fail: fail.clone(),
                samples: sample_tx.clone(),
            }))
        })
    };
    let recorded = notifications.clone();
    let watcher = MeetingWatcher::new(
        Duration::ZERO,
        Duration::from_millis(5),
        Some(Arc::new(move |platform| {
            detect_tx.send(platform.to_owned()).unwrap();
            Err(anyhow!("bad callback"))
        })),
    )
    .unwrap()
    .with_probe_factory(factory)
    .with_platform_detector(Arc::new(|| "Zoom".into()))
    .with_notifier(Arc::new(move |title, body| {
        recorded
            .lock()
            .unwrap()
            .push((title.to_owned(), body.to_owned()));
        Ok(())
    }));
    watcher.pause();
    watcher.start();
    watcher.start();
    assert!(sample_rx.recv_timeout(Duration::from_millis(30)).is_err());
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    watcher.resume();
    assert_eq!(
        detect_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "Zoom"
    );
    assert!(watcher.status().detected);
    watcher.pause();
    watcher.dismiss();
    assert!(!watcher.status().detected);
    assert_eq!(
        &*notifications.lock().unwrap(),
        &[(
            "localflow: meeting detected".into(),
            "Mic in use (Zoom). Open the localflow UI to transcribe.".into()
        )]
    );
    fail.store(true, Ordering::SeqCst);
    watcher.resume();
    while sample_rx.try_recv().is_ok() {}
    sample_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        detect_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "Zoom"
    );
    watcher.pause();
    assert!(watcher.status().error.is_empty());
    watcher.stop();
    watcher.start();
    watcher.resume();
    assert_eq!(
        detect_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "Zoom"
    );
    watcher.stop();
    assert!(!watcher.status().detected);
    assert!(!watcher.status().mic_busy);
    assert_eq!(watcher.status().platform, "Zoom");
}
