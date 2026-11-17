//! Offscreen shell, Home and History snapshots; no real user files or services.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("Native UI snapshots are only supported on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    native::run()
}

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{anyhow, ensure, Context, Result};
    use lf_core::config::Config;
    use lf_core::dictionary::DictionaryStore;
    use lf_core::history::{HistoryStore, NewEntry};
    use lf_ui::mac::hub::Hub;
    use lf_ui::mac::page::HubServices;
    use lf_ui::mac::snapshot::{set_appearance, snapshot_png, spin_run_loop, SnapshotTarget};
    use lf_ui::snapshot_plan::{
        history_rows, total_snapshots, SCENARIOS, SNAPSHOT_SIZES, SNAPSHOT_THEMES,
    };
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use std::path::PathBuf;
    use std::rc::Rc;

    pub fn run() -> Result<()> {
        let out: PathBuf = std::env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("artifacts/rust-ui"), PathBuf::from);
        std::fs::create_dir_all(&out)?;
        let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("must run on the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        let temp = tempfile::tempdir()?;
        let mut written = 0;
        for scenario in SCENARIOS {
            let base = temp.path().join(scenario.fixture_id());
            std::fs::create_dir_all(&base)?;
            let history = HistoryStore::open(
                Some(base.join("history.sqlite")),
                Some(base.join("clips")),
                true,
            )?;
            for row in history_rows(scenario.history) {
                let mut entry = NewEntry::new(row.kind);
                entry.app_name = Some(row.app.into());
                entry.raw_text = row.raw;
                entry.final_text = row.text;
                entry.duration_s = row.duration_s;
                if row.clip {
                    let clip = history.write_clip(&[0.0_f32; 1600], 16000)?;
                    entry.audio_path = Some(clip.to_string_lossy().into_owned());
                }
                history.add(&entry)?;
            }
            let mut config = Config::defaults_for(true);
            config.set_field("dictation_enabled", scenario.dictation_enabled.into());
            let services = HubServices::new_isolated(
                config,
                Rc::new(history),
                Rc::new(DictionaryStore::new(Some(base.join("dictionary.json")))),
            );
            let hub = Hub::new(mtm, services.clone(), scenario.page);
            if let Some(error) = scenario.error {
                services.set_page_error(scenario.page, error);
            }
            for (size, width, height) in SNAPSHOT_SIZES {
                hub.resize_content(width, height);
                for (theme, dark) in SNAPSHOT_THEMES {
                    set_appearance(SnapshotTarget::Window(hub.window()), dark)?;
                    hub.rebuild_current_page();
                    spin_run_loop(0.05);
                    let name = scenario.file_name(size, theme);
                    snapshot_png(SnapshotTarget::Window(hub.window()), &out.join(&name), dark)
                        .with_context(|| format!("snapshot {name}"))?;
                    written += 1;
                }
            }
            hub.shutdown();
        }
        ensure!(
            written == total_snapshots(),
            "unexpected snapshot count {written}"
        );
        println!("wrote {written} snapshots to {}", out.display());
        Ok(())
    }
}
