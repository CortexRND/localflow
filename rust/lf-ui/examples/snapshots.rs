//! Offscreen snapshots of the ported Hub pages; no real user files or services.

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
    use lf_ui::backends::prompts::WorkPrompt;
    use lf_ui::mac::hub::Hub;
    use lf_ui::mac::page::HubServices;
    use lf_ui::mac::snapshot::{set_appearance, snapshot_png, spin_run_loop, SnapshotTarget};
    use lf_ui::snapshot_plan::{
        dictionary_replacements, dictionary_words, history_rows, queue_prompts, total_snapshots,
        StyleStep, SCENARIOS, SNAPSHOT_SIZES, SNAPSHOT_THEMES,
    };
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSButton, NSView,
    };
    use serde_json::{Map, Value};
    use std::path::PathBuf;
    use std::rc::Rc;

    /// Every `NSButton` below `view` (depth first).
    fn buttons(view: &NSView, out: &mut Vec<Retained<NSButton>>) {
        for sub in view.subviews().iter() {
            if let Ok(button) = sub.clone().downcast::<NSButton>() {
                out.push(button);
            }
            buttons(&sub, out);
        }
    }

    /// Click the button whose accessibility label is `label`, through its real
    /// target/action (the Python script calls the same handlers).
    fn click(hub: &Hub, label: &str) -> Result<()> {
        let mut all = Vec::new();
        buttons(&hub.content_view(), &mut all);
        let button = all
            .iter()
            .find(|b| {
                b.accessibilityLabel()
                    .is_some_and(|l| l.to_string() == label)
            })
            .ok_or_else(|| anyhow!("no button labelled {label:?} on {}", hub.current_page()))?;
        // SAFETY: a plain click on a live button on the main thread.
        unsafe { button.performClick(None) };
        // The page asks for deferred rebuilds; let them run.
        spin_run_loop(0.1);
        Ok(())
    }

    /// The Style interactions of `_style_snapshots`.
    fn run_style_steps(
        hub: &Hub,
        services: &HubServices,
        step: StyleStep,
        base: &std::path::Path,
    ) -> Result<()> {
        if step == StyleStep::None {
            return Ok(());
        }
        // Writes stay in memory: the real config is never touched.
        if step == StyleStep::Empty {
            click(hub, "Cleanup level: None")?;
            return Ok(());
        }
        click(hub, "Cleanup level: Medium")?;
        click(hub, "Work style: Casual")?;
        if step == StyleStep::Populated {
            click(hub, "Email style: Casual")?;
            click(hub, "Personal style: Excited")?;
        }
        if step == StyleStep::SaveError {
            // A config path below a regular file cannot be created: the write
            // fails like Python's `OSError("read-only")` patch.
            let blocker = base.join("not-a-directory");
            std::fs::write(&blocker, b"x")?;
            *services.config_path.borrow_mut() = Some(blocker.join("config.toml"));
            services.memory_only_config.set(false);
            click(hub, "Email style: Excited")?;
            services.memory_only_config.set(true);
        }
        Ok(())
    }

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
            let dictionary = DictionaryStore::new(Some(base.join("dictionary.json")));
            for word in dictionary_words(scenario.dictionary) {
                dictionary.add_word(word)?;
            }
            for (from, to) in dictionary_replacements(scenario.dictionary) {
                dictionary.add_replacement(from, to)?;
            }
            let services = HubServices::new_isolated(config, Rc::new(history), Rc::new(dictionary));
            let prompts = queue_prompts(scenario.queue);
            let work: Vec<_> = prompts
                .iter()
                .map(|p| WorkPrompt::new(p.title, p.body))
                .collect();
            let paths: Vec<_> = (1..=work.len())
                .map(|n| {
                    base.join(format!("prompt-{n:02}.md"))
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            let entries = services
                .prompt_queue
                .add(&work, &paths, "Weekly planning")?;
            for (entry, prompt) in entries.iter().zip(&prompts) {
                if let Some(error) = prompt.failed_with {
                    let id = entry
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("queue entry missing id"))?;
                    let mut fields = Map::new();
                    fields.insert("repo".into(), "localflow".into());
                    fields.insert("error".into(), error.into());
                    services
                        .prompt_queue
                        .set_status(id, "failed", None, &fields)?;
                }
            }
            let hub = Hub::new(mtm, services.clone(), scenario.page);
            if let Some(error) = scenario.error {
                services.set_page_error(scenario.page, error);
            }
            run_style_steps(&hub, &services, scenario.style, &base)?;
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
