//! Idle footprint probe: wires the controller, worker, config watcher and
//! keyboard hub with null audio/speech (no listener, no menu bar) and
//! reports RSS and CPU time after sitting idle. Linux only (reads /proc).
//!
//! `cargo run --release -p lf-runtime --example idle -- 10`

use std::sync::Arc;
use std::time::Duration;

use lf_core::config::Config;
use lf_input::hub::KeyboardHub;
use lf_input::paste::SelectionCapture;
use lf_runtime::app::{hub_actions, job_sink, keyboard_bindings, work_queue, LiveSettings};
use lf_runtime::controller::{Controller, ControllerDeps};
use lf_runtime::engine::{AudioSource, Pcm, SpeechEngine, TextOutput};
use lf_runtime::live::ConfigWatcher;
use lf_runtime::pipeline::Vocabulary;
use lf_runtime::settings::load_runtime_config;
use lf_runtime::worker::Worker;

struct NullAudio;
impl AudioSource for NullAudio {
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn stop(&mut self) -> Pcm {
        Vec::new()
    }
    fn is_recording(&self) -> bool {
        false
    }
}

struct NullSpeech;
impl SpeechEngine for NullSpeech {
    fn load(&self) -> Result<(), String> {
        Ok(())
    }
    fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
        Ok(String::new())
    }
    fn unload(&self) {}
}

struct NullOutput;
impl TextOutput for NullOutput {
    fn paste_text(&self, _: &str, _: &str, _: Option<String>) {}
    fn capture_selection(&self) -> SelectionCapture {
        SelectionCapture {
            text: String::new(),
            clipboard_before: None,
        }
    }
    fn restore_clipboard_if_matches(&self, _: &str, _: Option<&str>) {}
}

fn proc_field(file: &str, key: &str) -> String {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with(key))
        .map(|l| l.trim_start_matches(key).trim().to_string())
        .unwrap_or_else(|| "n/a".into())
}

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    // utime + stime (fields 14 and 15; index 11/12 after the comm field)
    fields
        .get(11)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        + fields
            .get(12)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    lf_runtime::settings::set_dictation_enabled(&path, true).expect("config");
    let config = Arc::new(load_runtime_config(Some(&path)).unwrap_or_else(|_| Config::default()));
    let (work_tx, work_rx) = work_queue();
    let deps = ControllerDeps {
        audio: Box::new(NullAudio),
        jobs: job_sink(work_tx.clone()),
        output: Arc::new(NullOutput),
        play_sound: Arc::new(|_| {}),
        frontmost: Arc::new(|| None),
        on_state: None,
        latest_text: Arc::new(|f| f.to_string()),
        toggle_store: Arc::new(|| Ok(true)),
        clock: Arc::new(|| 0.0),
        min_clip_seconds: 0.3,
        initial_last_text: String::new(),
    };
    let (controller, consumer) = Controller::new(config.clone(), deps);
    let _consumer = consumer.spawn();
    let live = Arc::new(LiveSettings::new(
        config.clone(),
        controller.clone(),
        Some(work_tx),
        None,
    ));
    let worker_live = live.clone();
    let worker = Worker {
        config: Arc::new(move || worker_live.config()),
        vocabulary: Arc::new(|_| Vocabulary::default()),
        history: None,
        speech: Arc::new(NullSpeech),
        build_llm: Arc::new(|_| None),
        build_stt: Arc::new(|_| Ok(Arc::new(NullSpeech) as Arc<dyn SpeechEngine>)),
        deliver: Arc::new(|_, _, _| {}),
        last_text_path: None,
    };
    std::thread::spawn(move || worker.run(work_rx));
    let (hub, _) = KeyboardHub::new(
        &keyboard_bindings(&config, &controller),
        hub_actions(&controller),
    );
    live.set_hub(hub);
    let loader = path.clone();
    let apply = live.clone();
    let _watch = ConfigWatcher::new(
        path,
        Arc::new(move || load_runtime_config(Some(&loader))),
        Arc::new(move |c| {
            apply.apply(&c);
        }),
    )
    .start();
    let before = cpu_ticks();
    std::thread::sleep(Duration::from_secs(seconds));
    let after = cpu_ticks();
    println!(
        "idle {seconds}s: VmRSS {} | VmHWM {} | threads {} | cpu ticks {}",
        proc_field("/proc/self/status", "VmRSS:"),
        proc_field("/proc/self/status", "VmHWM:"),
        proc_field("/proc/self/status", "Threads:"),
        after - before
    );
}
