//! Renders every native Hub page and setup state to PNG files (macOS only).
//!
//! `cargo run -p lf-ui --example snapshots [OUT_DIR]` writes Python-compatible
//! filenames (default `artifacts/rust-ui`) for every scenario of
//! [`lf_ui::snapshot_plan`]. Hub pages use the minimum and 1280x820 sizes;
//! setup keeps Python's default names and adds wide counterparts. Everything runs against a
//! temporary directory and isolated fakes: no real config, history, dictionary,
//! keychain, network or microphone is read or written by fixture preparation.
//!
//! Seeding is driven by the scenario's seed fields (`lf_ui::snapshot_plan`),
//! never by comparing a state name with a fixture id.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("snapshots: the native Hub snapshots only run on macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = native::run() {
        eprintln!("snapshots: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;

    use anyhow::{anyhow, bail, Context, Result};
    use lf_core::config::Config;
    use lf_core::dictionary::DictionaryStore;
    use lf_core::history::{HistoryStore, NewEntry};
    use lf_ui::backends::meeting::FakeMeetingClient;
    use lf_ui::backends::prompts::{FakePromptQueue, PromptQueue, WorkPrompt};
    use lf_ui::mac::hub::Hub;
    use lf_ui::mac::page::HubServices;
    use lf_ui::mac::setup::{snapshot_fixture, SetupSnapshotState};
    use lf_ui::mac::snapshot::{set_appearance, snapshot_png, spin_run_loop, SnapshotTarget};
    use lf_ui::snapshot_plan::{
        dictionary_replacements, dictionary_words, history_rows, queue_prompts, setup_scenarios,
        total_setup_snapshots, total_snapshots, Scenario, SetupScenario, StyleStep, SCENARIOS,
        SNAPSHOT_SIZES, SNAPSHOT_THEMES,
    };
    use objc2::rc::Retained;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSButton, NSView,
    };
    use serde_json::{Map, Value};

    /// Real lf-core stores under `base` plus in-memory fakes, seeded from the
    /// scenario's seed fields. `base` holds one fixture directory per scenario
    /// (`Scenario::fixture_id`), so no two scenarios share data.
    struct Seeded {
        services: Rc<HubServices>,
        base: PathBuf,
    }

    fn seed(base: &Path, scenario: &Scenario) -> Result<Seeded> {
        let id = scenario.fixture_id();
        let history = HistoryStore::open(
            Some(base.join(format!("history-{id}.sqlite"))),
            Some(base.join(format!("clips-{id}"))),
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
        let dictionary = DictionaryStore::new(Some(base.join(format!("dictionary-{id}.json"))));
        for word in dictionary_words(scenario.dictionary) {
            dictionary.add_word(word)?;
        }
        for (from, to) in dictionary_replacements(scenario.dictionary) {
            dictionary.add_replacement(from, to)?;
        }
        let queue = FakePromptQueue::new();
        let prompts = queue_prompts(scenario.queue);
        if !prompts.is_empty() {
            let work: Vec<WorkPrompt> = prompts
                .iter()
                .map(|p| WorkPrompt::new(p.title, p.body))
                .collect();
            let paths: Vec<String> = (1..=work.len())
                .map(|n| {
                    base.join("prompts")
                        .join(format!("{id}-{n:02}.md"))
                        .to_string_lossy()
                        .into()
                })
                .collect();
            let entries = queue.add(&work, &paths, "Weekly planning")?;
            for (entry, prompt) in entries.iter().zip(&prompts) {
                let Some(error) = prompt.failed_with else {
                    continue;
                };
                let entry_id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("queue entry without an id"))?;
                let mut fields = Map::new();
                fields.insert("repo".into(), "localflow".into());
                fields.insert("error".into(), error.into());
                queue.set_status(entry_id, "failed", None, &fields)?;
            }
        }
        let services = HubServices::new_isolated(
            Config::default(),
            Rc::new(history),
            Rc::new(dictionary),
            Arc::new(queue),
            Arc::new(FakeMeetingClient::new()),
        );
        Ok(Seeded {
            services,
            base: base.to_path_buf(),
        })
    }

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
    fn run_style_steps(hub: &Hub, seeded: &Seeded, step: StyleStep) -> Result<()> {
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
            let blocker = seeded.base.join("not-a-directory");
            std::fs::write(&blocker, b"x")?;
            *seeded.services.config_path.borrow_mut() = Some(blocker.join("config.toml"));
            seeded.services.memory_only_config.set(false);
            click(hub, "Email style: Excited")?;
            seeded.services.memory_only_config.set(true);
        }
        Ok(())
    }

    fn setup_state(scenario: &SetupScenario) -> Result<SetupSnapshotState> {
        if let Some(step) = scenario.state.strip_prefix("step-") {
            let one_based: usize = step.parse().context("setup step number")?;
            return Ok(SetupSnapshotState::Step(one_based.saturating_sub(1)));
        }
        Ok(match (scenario.state, scenario.wide) {
            ("checks-needs-action", false) => SetupSnapshotState::ChecksNeedsAction,
            ("checks-all-ready", _) => SetupSnapshotState::ChecksAllReady,
            ("checks-needs-action", true) => SetupSnapshotState::ChecksNeedsActionWide,
            ("complete", _) => SetupSnapshotState::Complete,
            ("cleanup-level-medium", _) => SetupSnapshotState::CleanupLevelMedium,
            ("non-ollama-provider", _) => SetupSnapshotState::NonOllamaProvider,
            ("download-progress", _) => SetupSnapshotState::DownloadProgress,
            ("download-error", _) => SetupSnapshotState::DownloadError,
            other => bail!("unknown setup snapshot route {other:?}"),
        })
    }

    pub fn run() -> Result<()> {
        let out: PathBuf = std::env::args_os()
            .nth(1)
            .map_or_else(|| PathBuf::from("artifacts/rust-ui"), PathBuf::from);
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("must run on the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon, no menu bar takeover.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();

        // The temporary directory outlives every Hub and every render.
        let temp = tempfile::tempdir().context("creating the snapshot data directory")?;
        let mut written = 0usize;
        for scenario in &SCENARIOS {
            // Seed first, then build the Hub, so every page reads real data.
            let seeded = seed(temp.path(), scenario)?;
            let hub = Hub::new(mtm, seeded.services.clone(), scenario.page);
            let fixture_root = temp.path().join(scenario.fixture_id());
            std::fs::create_dir_all(&fixture_root)?;
            hub.prepare_snapshot(scenario.prepare, &fixture_root)?;
            hub.show_page(scenario.page);
            spin_run_loop(0.05);
            run_style_steps(&hub, &seeded, scenario.style)?;
            for (size, width, height) in SNAPSHOT_SIZES {
                hub.resize_content(width, height);
                for (theme, dark) in SNAPSHOT_THEMES {
                    set_appearance(SnapshotTarget::Window(hub.window()), dark)?;
                    // The palette is re-read on every render: rebuild.
                    hub.rebuild_current_page();
                    spin_run_loop(0.05);
                    let name = scenario.file_name(size, theme);
                    let path = out.join(&name);
                    snapshot_png(SnapshotTarget::Window(hub.window()), &path, dark)
                        .with_context(|| format!("snapshot {name}"))?;
                    println!("wrote {}", path.display());
                    written += 1;
                }
            }
            hub.shutdown();
        }
        let setup = snapshot_fixture(mtm);
        for scenario in setup_scenarios() {
            setup.seed_snapshot(setup_state(&scenario)?);
            setup.resize_content(
                if scenario.wide {
                    1100.0
                } else {
                    f64::from(lf_ui::tokens::WINDOW_MIN_WIDTH)
                },
                f64::from(lf_ui::tokens::WINDOW_MIN_HEIGHT),
            );
            for (theme, dark) in SNAPSHOT_THEMES {
                set_appearance(SnapshotTarget::Window(setup.window()), dark)?;
                setup.rebuild();
                spin_run_loop(0.05);
                let name = scenario.file_name(theme);
                let path = out.join(&name);
                snapshot_png(SnapshotTarget::Window(setup.window()), &path, dark)
                    .with_context(|| format!("snapshot {name}"))?;
                println!("wrote {}", path.display());
                written += 1;
            }
        }
        setup.close();

        let expected = total_snapshots() + total_setup_snapshots();
        if written != expected {
            bail!("wrote {written} snapshots, expected {expected}");
        }
        println!("wrote {written} snapshots to {}", out.display());
        drop(temp);
        Ok(())
    }
}
