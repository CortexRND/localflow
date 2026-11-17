use anyhow::Result;
use lf_core::config::Config;
use lf_meetings::MeetingWatcher;
use lf_server::backend::{Backend, BackendRequest};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use std::time::Duration;

type BackendSlot = Arc<Mutex<Option<Weak<dyn Backend>>>>;

fn enabled(config: &Config, field: &str) -> bool {
    config.field_bool(field).unwrap_or(false)
}

fn detected(
    slot: &BackendSlot,
    live: &Mutex<Config>,
    stopping: &AtomicBool,
    platform: &str,
) -> Result<()> {
    let config = live.lock().unwrap().clone();
    if stopping.load(Ordering::Acquire)
        || !["meetings_enabled", "meeting_watch", "meeting_auto_start"]
            .iter()
            .all(|field| enabled(&config, field))
    {
        return Ok(());
    }
    if let Some(backend) = slot.lock().unwrap().as_ref().and_then(Weak::upgrade) {
        let title = if platform.is_empty() || platform == "Other" {
            "Meeting".into()
        } else {
            format!("{platform} meeting")
        };
        let response = backend.handle(BackendRequest {
            method: "POST".parse()?,
            path: "/meeting/start".into(),
            query: Default::default(),
            headers: Default::default(),
            body: serde_json::to_vec(
                &serde_json::json!({"title": title, "category":"", "auto_title":true}),
            )?
            .into(),
            config,
        });
        anyhow::ensure!(
            response.status == 200 || response.status == 409,
            "meeting auto-start failed ({})",
            response.status
        );
    }
    Ok(())
}

fn sync(watcher: &MeetingWatcher, live: &Mutex<Config>, fresh: Config) {
    let watch = enabled(&fresh, "meetings_enabled") && enabled(&fresh, "meeting_watch");
    *live.lock().unwrap() = fresh;
    if watch {
        watcher.start();
    } else {
        watcher.stop();
    }
}

pub(super) struct MeetingWatch {
    pub watcher: Arc<MeetingWatcher>,
    slot: BackendSlot,
    live: Arc<Mutex<Config>>,
    stopping: Arc<AtomicBool>,
}

impl MeetingWatch {
    pub fn new(config: Config) -> Result<Self> {
        Self::configured(config, Duration::from_secs(3), |watcher| watcher)
    }

    fn configured(
        config: Config,
        poll: Duration,
        configure: impl FnOnce(MeetingWatcher) -> MeetingWatcher,
    ) -> Result<Self> {
        let min_busy = config
            .field_f64("meeting_min_busy_seconds")
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .unwrap_or(Duration::from_secs(12));
        let live = Arc::new(Mutex::new(config));
        let slot: BackendSlot = Arc::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (callback_slot, callback_live, callback_stopping) =
            (slot.clone(), live.clone(), stopping.clone());
        let watcher = MeetingWatcher::new(
            min_busy,
            poll,
            Some(Arc::new(move |platform| {
                detected(&callback_slot, &callback_live, &callback_stopping, platform)
            })),
        )?;
        #[cfg(target_os = "macos")]
        let watcher = watcher.with_notifier(Arc::new(|title, message| {
            let escape = |value: &str| {
                value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
            };
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                escape(message),
                escape(title)
            );
            std::process::Command::new("/usr/bin/osascript")
                .args(["-e", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            Ok(())
        }));
        Ok(Self {
            watcher: Arc::new(configure(watcher)),
            slot,
            live,
            stopping,
        })
    }

    pub fn attach(&self, backend: &Arc<dyn Backend>) {
        *self.slot.lock().unwrap() = Some(Arc::downgrade(backend));
    }

    pub fn apply_callback(&self) -> Arc<dyn Fn(Config) + Send + Sync> {
        let (watcher, live) = (self.watcher.clone(), self.live.clone());
        Arc::new(move |fresh| sync(&watcher, &live, fresh))
    }

    pub fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.watcher.stop();
    }
}

impl Drop for MeetingWatch {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_server::backend::BackendResponse;
    use serde_json::json;

    struct Requests(
        Mutex<Vec<BackendRequest>>,
        Option<std::sync::mpsc::Sender<()>>,
    );
    impl Backend for Requests {
        fn handle(&self, request: BackendRequest) -> BackendResponse {
            self.0.lock().unwrap().push(request);
            if let Some(done) = &self.1 {
                let _ = done.send(());
            }
            BackendResponse {
                status: 200,
                body: json!({}),
            }
        }
    }
    struct Probe(bool);
    impl lf_meetings::watcher::MicUsageProbe for Probe {
        fn mic_in_use(&self) -> Result<bool> {
            Ok(self.0)
        }
    }

    #[test]
    fn busy_probe_reaches_the_registered_auto_record_callback() {
        let mut config = Config::default();
        for field in ["meetings_enabled", "meeting_watch", "meeting_auto_start"] {
            config.set_field(field, json!(true));
        }
        config.set_field("meeting_min_busy_seconds", json!(0));
        let service =
            MeetingWatch::configured(config.clone(), Duration::from_millis(1), |watcher| {
                watcher
                    .with_probe_factory(Arc::new(|| Ok(Box::new(Probe(true)))))
                    .with_platform_detector(Arc::new(|| "Zoom".into()))
                    .with_notifier(Arc::new(|_, _| Ok(())))
            })
            .unwrap();
        let (done, received) = std::sync::mpsc::channel();
        let requests = Arc::new(Requests(Mutex::new(vec![]), Some(done)));
        let backend: Arc<dyn Backend> = requests.clone();
        service.attach(&backend);
        sync(&service.watcher, &service.live, config);
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(requests.0.lock().unwrap()[0].path, "/meeting/start");
    }

    #[test]
    fn watcher_attaches_syncs_toggles_and_auto_starts_only_when_enabled() {
        let mut config = Config::default();
        for field in ["meetings_enabled", "meeting_watch", "meeting_auto_start"] {
            config.set_field(field, json!(true));
        }
        let mut service =
            MeetingWatch::configured(config.clone(), Duration::from_secs(3), |watcher| {
                watcher.with_probe_factory(Arc::new(|| Ok(Box::new(Probe(false)))))
            })
            .unwrap();
        let requests = Arc::new(Requests(Mutex::new(vec![]), None));
        let backend: Arc<dyn Backend> = requests.clone();
        service.attach(&backend);
        let meetings = lf_meetings_backend::MeetingsBackend::new()
            .unwrap()
            .with_watcher(service.watcher.clone());
        sync(&service.watcher, &service.live, config.clone());
        let response = meetings.handle(BackendRequest {
            method: "GET".parse().unwrap(),
            path: "/meeting/status".into(),
            body: Default::default(),
            headers: Default::default(),
            query: Default::default(),
            config: config.clone(),
        });
        assert_eq!(response.body["watching"], true);
        assert_eq!(response.body["watch_error"], "");
        for field in ["meetings_enabled", "meeting_watch", "meeting_auto_start"] {
            let mut disabled = config.clone();
            disabled.set_field(field, json!(false));
            sync(&service.watcher, &service.live, disabled);
            detected(&service.slot, &service.live, &service.stopping, "Zoom").unwrap();
        }
        assert!(requests.0.lock().unwrap().is_empty());
        sync(&service.watcher, &service.live, config);
        for platform in ["Zoom", "Other"] {
            detected(&service.slot, &service.live, &service.stopping, platform).unwrap();
        }
        let recorded = requests.0.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        for (request, title) in recorded.iter().zip(["Zoom meeting", "Meeting"]) {
            assert_eq!(request.path, "/meeting/start");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["title"], title);
            assert_eq!(body["auto_title"], true);
        }
        drop(recorded);
        service.shutdown();
        detected(&service.slot, &service.live, &service.stopping, "Zoom").unwrap();
        assert_eq!(requests.0.lock().unwrap().len(), 2);
        assert!(!service.watcher.status().detected);
    }
}
