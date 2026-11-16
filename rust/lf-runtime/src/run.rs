//! The dictation app's entry point (`app.main`): one process holding the
//! single-instance lock, the controller and worker threads, the config
//! watcher, the keyboard hub and (on macOS) the menu bar.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use lf_core::config::Config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use lf_core::hotphrases::HotPhraseStore;
use lf_core::symbols::discover_commands;
use lf_input::hub::KeyboardHub;
use lf_input::paste::Injector;
use lf_providers::factory::{build_llm, BuildOptions};
use lf_providers::CoreSecrets;

use crate::app::{hub_actions, job_sink, keyboard_bindings, work_queue, LiveSettings};
use crate::controller::{Controller, ControllerDeps, MIN_CLIP_SECONDS};
use crate::engine::{AudioSource, SpeechEngine};
use crate::live::ConfigWatcher;
#[cfg(target_os = "macos")]
use crate::menubar::MenuActions;
use crate::menubar::MenuState;
use crate::pipeline::Vocabulary;
use crate::settings::{bool_field, dictation_enabled, load_runtime_config, toggle_dictation};
use crate::singleinstance::{acquire, read_holder};
use crate::status::{last_text_path, read_last_text, status_path, write_state};
use crate::worker::{LlmFactory, SttFactory, VocabularyFn, Worker};

pub struct RunOptions {
    pub audio: Box<dyn AudioSource>,
    pub speech: Arc<dyn SpeechEngine>,
    pub build_stt: SttFactory,
    pub play_sound: Arc<dyn Fn(&str) + Send + Sync>,
}

/// Dictionary, hot phrases and spoken commands from the user's stores. Commands
/// are discovered once; the dictionary and phrases are read per job.
pub fn default_vocabulary(config: &Config) -> VocabularyFn {
    let dictionary = DictionaryStore::new(None);
    let phrases = HotPhraseStore::default_store();
    let commands = discover_commands(
        &config.field_str_list("command_names").unwrap_or_default(),
        &config.field_str_list("skills_dirs").unwrap_or_default(),
    );
    Arc::new(move |config| Vocabulary {
        words: dictionary.words(),
        replacements: dictionary.replacements(),
        hot_phrases: if bool_field(config, "hot_phrases", true) {
            phrases.enabled().unwrap_or_default()
        } else {
            Vec::new()
        },
        commands: commands.clone(),
    })
}

pub fn default_llm_factory() -> LlmFactory {
    Arc::new(|config| build_llm(config, &BuildOptions::default(), &CoreSecrets).ok())
}

/// Run until the keyboard listener (or menu bar) exits.
pub fn run(options: RunOptions) -> Result<()> {
    let _lock = match acquire("localflow")? {
        Some(lock) => lock,
        None => {
            let (pid, since) = read_holder("localflow");
            return Err(anyhow!(
                "another localflow instance is already running (pid {}, since {})",
                pid.map_or("unknown".into(), |p| p.to_string()),
                since.unwrap_or_else(|| "unknown time".into())
            ));
        }
    };
    let config_path = lf_core::config::try_config_path()?;
    let config = Arc::new(load_runtime_config(None)?);
    options
        .speech
        .load()
        .map_err(|e| anyhow!("speech model failed to load: {e}"))?;
    let history_enabled = bool_field(&config, "history_enabled", true);
    let _ = write_state("idle", &status_path());

    let menu = MenuState::new(crate::platform_menu_bar(), dictation_enabled(&config));
    let (work_tx, work_rx) = work_queue();
    let state_menu = menu.clone();
    let toggle_path = config_path.clone();
    let output: Arc<Injector> = Arc::new(Injector::native());
    let deps = ControllerDeps {
        audio: options.audio,
        jobs: job_sink(work_tx.clone()),
        output: output.clone(),
        play_sound: options.play_sound.clone(),
        frontmost: Arc::new(crate::context::frontmost_app),
        on_state: Some(Arc::new(move |state: &str| {
            let _ = write_state(state, &status_path());
            let state = state.to_string();
            state_menu.change(|m| m.app_state = state);
        })),
        latest_text: Arc::new(move |fallback: &str| {
            HistoryStore::open(None, None, history_enabled)
                .and_then(|h| h.latest_text(fallback))
                .unwrap_or_else(|_| fallback.to_string())
        }),
        toggle_store: Arc::new(move || toggle_dictation(&toggle_path).map_err(|e| e.to_string())),
        clock: {
            let start = std::time::Instant::now();
            Arc::new(move || start.elapsed().as_secs_f64())
        },
        min_clip_seconds: MIN_CLIP_SECONDS,
        initial_last_text: read_last_text(&last_text_path()),
    };
    let (controller, consumer) = Controller::new(config.clone(), deps);
    let consumer_thread = consumer.spawn();

    let live = Arc::new(LiveSettings::new(
        config.clone(),
        controller.clone(),
        Some(work_tx.clone()),
        Some(menu.clone()),
    ));

    let worker_live = live.clone();
    let deliver_controller = controller.clone();
    let worker = Worker {
        config: Arc::new(move || worker_live.config()),
        vocabulary: default_vocabulary(&config),
        history: HistoryStore::open(None, None, history_enabled).ok(),
        speech: options.speech,
        build_llm: default_llm_factory(),
        build_stt: options.build_stt,
        deliver: Arc::new(move |job, text, error| {
            deliver_controller.note_job_result(
                text,
                job.clipboard_before.clone(),
                error,
                Some(job.selection.clone()),
            )
        }),
        last_text_path: Some(last_text_path()),
    };
    let worker_thread = std::thread::Builder::new()
        .name("localflow-worker".into())
        .spawn(move || worker.run(work_rx))?;

    let (hub, warnings) = KeyboardHub::new(
        &keyboard_bindings(&config, &controller),
        hub_actions(&controller),
    );
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    live.set_hub(hub.clone());

    let loader_path = config_path.clone();
    let apply_live = live.clone();
    let tick_live = live.clone();
    let watcher = ConfigWatcher::new(
        config_path,
        Arc::new(move || load_runtime_config(Some(&loader_path))),
        Arc::new(move |fresh| {
            apply_live.apply(&fresh);
        }),
    )
    .with_tick(
        Arc::new(move || tick_live.retry()),
        crate::live::DEFAULT_INTERVAL,
    );
    let _watch = watcher.start();
    // An edit made while the speech model loaded predates the watcher's
    // baseline signature: apply the file as it is now (a no-op if unchanged).
    match load_runtime_config(None) {
        Ok(fresh) => {
            live.apply(&fresh);
        }
        Err(e) => log::warn!("config reload skipped: {e}"),
    }

    let warn_menu = menu.clone();
    let warn: lf_input::listener::WarningSink = Arc::new(move |w| {
        eprintln!("warning: {w}");
        warn_menu.change(|m| m.input_warning = Some("⚠︎ Input Monitoring needed".into()));
    });

    let finish = {
        let controller = controller.clone();
        let work_tx = work_tx.clone();
        move || {
            controller.shutdown();
            let _ = work_tx.send(crate::worker::WorkItem::Shutdown);
        }
    };

    #[cfg(target_os = "macos")]
    {
        let listener_hub = hub.clone();
        let listener_warn = warn.clone();
        std::thread::Builder::new()
            .name("localflow-keyboard".into())
            .spawn(move || {
                if let Err(e) = lf_input::listener::run_forever(&listener_hub, listener_warn) {
                    log::error!("{e}");
                }
                log::error!(
                    "hotkey listener exited unexpectedly — exiting non-zero for launchd to restart"
                );
                std::process::exit(1);
            })?;
        let toggle = controller.clone();
        let paste = controller.clone();
        let actions = MenuActions {
            toggle_dictation: Arc::new(move || toggle.note_dictation_toggle()),
            paste_last: Arc::new(move || paste.note_paste_last()),
            quit: Arc::new(move || std::process::exit(0)),
        };
        let result = crate::macos::run_menu_bar(menu.snapshot(), actions);
        finish();
        let _ = consumer_thread.join();
        let _ = worker_thread.join();
        result.map_err(|e| anyhow!(e))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let result = lf_input::listener::run_forever(&hub, warn);
        finish();
        let _ = consumer_thread.join();
        let _ = worker_thread.join();
        drop(output);
        result.map_err(|e| anyhow!(e))
    }
}
