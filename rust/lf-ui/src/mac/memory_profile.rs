//! Opt-in CI instrumentation. The baseline receives this same driver only.
use super::{actions::Timer, app::WindowRouter};
use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(super) fn start(router: Rc<WindowRouter>) -> Option<Timer> {
    let output = PathBuf::from(std::env::var_os("LF_HUB_RSS_PROFILE")?);
    let mut stages = vec![("Home".to_string(), "Home")];
    for &page in crate::hub_route::HUB_PAGES.iter() {
        if page != "Home" {
            stages.push((page.to_string(), page));
            stages.push((format!("Home_after_{page}"), "Home"));
        }
    }
    stages.push(("Closed".into(), ""));
    stages.push(("Home_reopened".into(), "Home"));
    let index = Cell::new(0);
    let countdown = Cell::new(0);
    let sampling = Arc::new(AtomicBool::new(false));
    Some(Timer::schedule(1.0, true, move || {
        if sampling.load(Ordering::Acquire) {
            return;
        }
        let i = index.get();
        if i == stages.len() * 2 {
            std::fs::write(output.with_extension("done"), "done\n")
                .expect("write profile completion");
            index.set(i + 1);
            return;
        }
        if i > stages.len() * 2 {
            return;
        }
        if countdown.get() != 0 {
            countdown.set(countdown.get() - 1);
            return;
        }
        let (name, page) = &stages[i / 2];
        if i.is_multiple_of(2) {
            if page.is_empty() {
                if let Some(hub) = router.hub() {
                    hub.window().performClose(None);
                }
            } else {
                router.show(Some(page)).expect("profile page navigation");
            }
            countdown.set(4);
            index.set(i + 1);
        } else {
            if let Some(hub) = router.hub().filter(|hub| !hub.is_closed()) {
                if hub.drain_requests() != 0 {
                    countdown.set(1);
                    return;
                }
            }
            sampling.store(true, Ordering::Release);
            let sampling = sampling.clone();
            let output = output.clone();
            let name = name.clone();
            std::thread::spawn(move || {
                use std::{io::Write, process::Command};
                let client = reqwest::blocking::Client::builder()
                    .no_proxy()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap();
                let base = "http://127.0.0.1:8756";
                let health: serde_json::Value = client
                    .get(format!("{base}/healthz"))
                    .send()
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .unwrap();
                assert_eq!(health["ok"].as_bool(), Some(true));
                let token =
                    lf_core::apitoken::load_at(&lf_core::apitoken::try_token_path().unwrap())
                        .unwrap();
                let status: serde_json::Value = client
                    .get(format!("{base}/api/status"))
                    .header("X-Localflow-Token", token.trim())
                    .send()
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .unwrap();
                assert_eq!(status["stt"]["loaded"].as_bool(), Some(false));
                let pid = std::process::id().to_string();
                let metrics =
                    Command::new(std::env::var_os("LF_MEMORY_PROBE").expect("memory probe"))
                        .arg(&pid)
                        .output()
                        .expect("process memory probe");
                assert!(metrics.status.success());
                let metrics: serde_json::Value = serde_json::from_slice(&metrics.stdout).unwrap();
                let rss = metrics["rss"].as_u64().unwrap() / 1024;
                let footprint = metrics["footprint"].as_u64().unwrap() / 1024;
                assert!(rss > 0 && footprint > 0);
                let mut report = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&output)
                    .unwrap();
                writeln!(report, "{name},{rss},{footprint}").unwrap();
                let prefix = output.parent().unwrap().join(name.replace(' ', "_"));
                for (tool, args) in [
                    ("vmmap", vec!["--summary", &pid]),
                    ("heap", vec!["-s", &pid]),
                ] {
                    match Command::new(tool).args(args).output() {
                        Ok(result) => {
                            let mut data = result.stdout;
                            data.extend(result.stderr);
                            std::fs::write(prefix.with_extension(format!("{tool}.txt")), data)
                                .unwrap();
                        }
                        Err(error) => eprintln!("profile {tool}: {error}"),
                    }
                }
                eprintln!("RSS_PROFILE {name}: RSS {rss} KiB; footprint {footprint} KiB");
                sampling.store(false, Ordering::Release);
            });
            index.set(i + 1);
        }
    }))
}
