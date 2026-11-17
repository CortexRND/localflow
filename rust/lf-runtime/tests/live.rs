//! Mirrors tests/test_live_config.py and tests/test_runtime_reload.py.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{config, rig_with};
use lf_input::hub::KeyboardHub;
use lf_input::Key;
use lf_runtime::app::{hub_actions, keyboard_bindings, LiveSettings};
use lf_runtime::live::{adopt_groups, merge_live, runtime_changes, ConfigWatcher};
use lf_runtime::settings::{dictation_enabled, load_runtime_config, set_dictation_enabled};
use lf_runtime::worker::WorkItem;
use serde_json::json;

#[test]
fn disabling_dictation_closes_audio_and_replaces_held_bindings_with_only_toggle() {
    let mut cfg = config();
    cfg.set_field("hotkey", json!("f19"));
    cfg.set_field("dictation_toggle_hotkey", json!("Control+Option+d"));
    let mut r = rig_with(cfg.clone());
    let (hub, _) = KeyboardHub::new(&keyboard_bindings(&cfg, &r.handle), hub_actions(&r.handle));
    let live = LiveSettings::new(Arc::new(cfg.clone()), r.handle.clone(), None, None);
    live.set_hub(hub.clone());
    hub.press(Key::from_name("f19").unwrap());
    r.controller.drain();
    assert!(r.recording());
    let mut disabled = cfg.clone();
    disabled.set_field("dictation_enabled", json!(false));
    live.apply(&disabled);
    r.controller.drain();
    assert!(!r.recording());
    assert_eq!(r.audio.0.lock().unwrap().stop_calls, 1);
    assert_eq!(r.jobs(), 0);
    assert!(!hub.reconfigure_pending());
    let bindings = keyboard_bindings(&disabled, &r.handle);
    assert!(bindings.hold_key.is_empty());
    assert!(bindings.toggle_combo.is_empty());
    assert!(bindings.command_key.is_empty());
    assert_eq!(bindings.combos.len(), 1);
    hub.release(Key::from_name("f19").unwrap());
    hub.press(Key::from_name("f19").unwrap());
    r.controller.drain();
    assert_eq!(r.start_calls(), 1);
    hub.release(Key::from_name("f19").unwrap());
    live.apply(&cfg);
    r.controller.drain();
    hub.press(Key::from_name("f19").unwrap());
    r.controller.drain();
    assert!(r.recording());
    assert_eq!(r.start_calls(), 2);
}

#[test]
fn disabled_without_toggle_chord_registers_nothing() {
    let mut cfg = config();
    cfg.set_field("dictation_enabled", json!(false));
    cfg.set_field("dictation_toggle_hotkey", json!(""));
    let r = rig_with(cfg.clone());
    let bindings = keyboard_bindings(&cfg, &r.handle);
    assert!(bindings.combos.is_empty());
    assert!(bindings.hold_key.is_empty());
}

#[test]
fn merge_live_takes_only_allowlisted_fields() {
    let running = config();
    let mut fresh = running.clone();
    fresh.set_field("sounds_enabled", json!(false));
    fresh.set_field("sample_rate", json!(48000));
    fresh.set_field("dictation_enabled", json!(false));
    let (merged, changed) = merge_live(&running, &fresh, &BTreeSet::new());
    assert_eq!(
        changed,
        ["dictation_enabled", "sounds_enabled"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    assert_eq!(merged.field("sample_rate"), running.field("sample_rate"));
    assert_eq!(merged.field("sounds_enabled"), Some(&json!(false)));
}

#[test]
fn model_groups_move_whole_never_field_by_field() {
    let running = config();
    let mut fresh = running.clone();
    fresh.set_field("llm_provider", json!("openai-compatible"));
    fresh.set_field("llm_model", json!("gpt"));
    fresh.set_field("llm_key_ref", json!("openai"));
    let groups = runtime_changes(&running, &fresh);
    assert_eq!(groups, ["llm"].into_iter().collect());
    let adopted = adopt_groups(&running, &fresh, &groups);
    for f in ["llm_provider", "llm_model", "llm_key_ref"] {
        assert_eq!(adopted.field(f), fresh.field(f));
    }
    assert_eq!(adopted.field("model_size"), running.field("model_size"));
}

#[test]
fn live_settings_reload_dictation_switch_and_models() {
    let mut r = rig_with(config());
    let (tx, rx) = std::sync::mpsc::channel();
    let live = LiveSettings::new(Arc::new(config()), r.handle.clone(), Some(tx), None);
    let mut fresh = config();
    fresh.set_field("dictation_enabled", json!(false));
    fresh.set_field("model_size", json!("small"));
    fresh.set_field("llm_model", json!("other"));
    let changed = live.apply(&fresh);
    assert!(changed.contains("dictation_enabled") && changed.contains("model_size"));
    r.controller.drain();
    assert!(!dictation_enabled(&r.handle.config()));
    let reloads: Vec<&str> = rx
        .try_iter()
        .map(|w| match w {
            WorkItem::ReloadLlm(_) => "llm",
            WorkItem::ReloadStt(_) => "stt",
            _ => "other",
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    assert_eq!(reloads, vec!["llm", "stt"]);
    // Off now: the hotkey is ignored.
    r.handle.note_down();
    r.controller.drain();
    assert_eq!(r.start_calls(), 0);
    // An unchanged reload does nothing.
    assert!(live.apply(&fresh).is_empty());
}

#[test]
fn shortcut_change_rebinds_the_running_hub_after_the_gesture() {
    let mut cfg = config();
    cfg.set_field("hotkey", json!("f19"));
    let mut r = rig_with(cfg.clone());
    let live = LiveSettings::new(Arc::new(cfg.clone()), r.handle.clone(), None, None);
    let (hub, _) = KeyboardHub::with_options(
        &keyboard_bindings(&cfg, &r.handle),
        hub_actions(&r.handle),
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    );
    live.set_hub(hub.clone());

    hub.press(Key::from_name("f19").unwrap());
    r.controller.drain();
    assert!(r.recording());
    let mut fresh = cfg.clone();
    fresh.set_field("hotkey", json!("f18"));
    live.apply(&fresh);
    r.controller.drain();
    // Busy recording: the rebind waits.
    assert!(hub.reconfigure_pending());
    r.set_now(2.0);
    hub.release(Key::from_name("f19").unwrap());
    r.controller.drain();
    assert!(!r.recording());
    assert_eq!(r.jobs(), 1);
    assert!(live.retry() || !hub.reconfigure_pending());
    assert!(!hub.reconfigure_pending());
    hub.press(Key::from_name("f18").unwrap());
    r.controller.drain();
    assert!(r.recording());
}

#[test]
fn dictation_toggle_shortcut_reaches_the_controller() {
    let mut cfg = config();
    cfg.set_field("dictation_toggle_hotkey", json!("Control+Option+d"));
    cfg.set_field("dictation_enabled", json!(false));
    let mut r = rig_with(cfg.clone());
    let (hub, warnings) = KeyboardHub::with_options(
        &keyboard_bindings(&cfg, &r.handle),
        hub_actions(&r.handle),
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    for k in ["alt", "ctrl", "d"] {
        hub.press(Key::from_name(k).unwrap());
    }
    r.controller.drain();
    assert!(dictation_enabled(&r.handle.config()));
    assert_eq!(r.start_calls(), 0);
}

#[test]
fn toggle_shortcut_clashing_with_polish_is_rejected() {
    let mut cfg = config();
    cfg.set_field("dictation_toggle_hotkey", json!("<alt>+<ctrl>+1"));
    let r = rig_with(cfg.clone());
    let (_, warnings) = KeyboardHub::with_options(
        &keyboard_bindings(&cfg, &r.handle),
        hub_actions(&r.handle),
        Arc::new(|| 0.0),
        Duration::from_secs(3600),
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].to_string().contains("<alt>+<ctrl>+1"));
}

#[test]
fn watcher_notifies_once_per_change_and_survives_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    set_dictation_enabled(&path, true).unwrap();
    let seen: Arc<Mutex<Vec<bool>>> = Default::default();
    let sink = seen.clone();
    let loader_path = path.clone();
    let watcher = ConfigWatcher::new(
        path.clone(),
        Arc::new(move || load_runtime_config(Some(&loader_path))),
        Arc::new(move |c| sink.lock().unwrap().push(dictation_enabled(&c))),
    );
    assert!(!watcher.check());
    std::thread::sleep(Duration::from_millis(20));
    set_dictation_enabled(&path, false).unwrap();
    assert!(watcher.check());
    assert!(!watcher.check());
    std::fs::write(&path, "[dictation\nenabled = = nope\n").unwrap();
    assert!(!watcher.check());
    assert_eq!(*seen.lock().unwrap(), vec![false]);
}

#[test]
fn watcher_thread_reacts_to_file_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    set_dictation_enabled(&path, true).unwrap();
    let seen: Arc<Mutex<Vec<bool>>> = Default::default();
    let sink = seen.clone();
    let loader_path = path.clone();
    let watcher = ConfigWatcher::new(
        path.clone(),
        Arc::new(move || load_runtime_config(Some(&loader_path))),
        Arc::new(move |c| sink.lock().unwrap().push(dictation_enabled(&c))),
    );
    let guard = watcher.start();
    std::thread::sleep(Duration::from_millis(100));
    set_dictation_enabled(&path, false).unwrap();
    for _ in 0..100 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(guard);
    assert_eq!(*seen.lock().unwrap(), vec![false]);
}
