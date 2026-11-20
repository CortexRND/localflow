//! Wiring: shortcut bindings, live settings and the dictation app's
//! threads (`localflow/app.py` `keyboard_bindings`, `LiveSettings`, `main`).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use lf_core::config::Config;
use lf_input::hub::{Callback, HubActions, KeyboardHub, Shortcuts};
use lf_input::is_hold_key;

use crate::controller::{ControllerHandle, Job};
use crate::live::{
    adopt_groups, merge_live, runtime_changes, runtime_fields, HOTKEY_FIELDS, RUNTIME_GROUPS,
};
use crate::menubar::MenuState;
use crate::settings::{dictation_enabled, str_field, DICTATION_TOGGLE_HOTKEY};
use crate::worker::WorkItem;

/// KeyboardHub bindings for `config` (construction and rebinds).
pub fn keyboard_bindings(config: &Config, controller: &ControllerHandle) -> Shortcuts {
    let hotkey = str_field(config, "hotkey");
    let hold = is_hold_key(&hotkey);
    let c = controller.clone();
    let paste_last: Callback = Arc::new(move || c.note_paste_last());
    let c = controller.clone();
    let polish: Callback = Arc::new(move || c.note_transform("polish"));
    let c = controller.clone();
    let prompt: Callback = Arc::new(move || c.note_transform("prompt"));
    let c = controller.clone();
    let toggle: Callback = Arc::new(move || c.note_dictation_toggle());
    if !dictation_enabled(config) {
        let chord = str_field(config, DICTATION_TOGGLE_HOTKEY);
        return Shortcuts {
            hold_key: String::new(),
            toggle_combo: String::new(),
            command_key: String::new(),
            combos: if chord.is_empty() {
                Vec::new()
            } else {
                vec![(chord, toggle)]
            },
        };
    }
    let combos = [
        (str_field(config, "paste_last_hotkey"), paste_last),
        (str_field(config, "polish_hotkey"), polish),
        (str_field(config, "prompt_hotkey"), prompt),
        (str_field(config, DICTATION_TOGGLE_HOTKEY), toggle),
    ]
    .into_iter()
    .filter(|(shortcut, _)| !shortcut.is_empty())
    .collect();
    Shortcuts {
        hold_key: if hold { hotkey.clone() } else { String::new() },
        toggle_combo: if hold { String::new() } else { hotkey },
        combos,
        command_key: str_field(config, "command_hotkey"),
    }
}

/// The hub's callbacks: enqueue-only controller notes.
pub fn hub_actions(controller: &ControllerHandle) -> HubActions {
    let note = |f: fn(&ControllerHandle)| -> Callback {
        let c = controller.clone();
        Arc::new(move || f(&c))
    };
    let idle = controller.clone();
    HubActions {
        on_down: note(ControllerHandle::note_down),
        on_up: note(ControllerHandle::note_up),
        on_toggle: note(ControllerHandle::note_toggle),
        on_cancel: note(ControllerHandle::note_cancel),
        on_command_down: note(ControllerHandle::note_command_down),
        on_command_up: note(ControllerHandle::note_command_up),
        on_command_cancel: note(ControllerHandle::note_command_cancel),
        can_reconfigure: Arc::new(move || idle.is_idle()),
    }
}

/// The keyboard listener exited without being asked to stop: shortcuts are
/// gone. Held keys are let go at the hub (idempotent; the Linux source has
/// already done it), so a hold ends normally and is transcribed while an
/// unfinished command is cancelled, never submitted. Nothing could end a
/// hands-free (toggle/locked) recording any more, so it is discarded; the
/// cancel is a no-op when idle.
pub fn listener_lost(hub: &KeyboardHub, controller: &ControllerHandle) {
    hub.end_of_input();
    controller.note_cancel();
}

/// The running app's config, refreshed when the file changes. Live fields
/// merge in place; model groups are adopted whole and their clients
/// rebuilt on the worker between jobs; shortcuts rebind on the one hub,
/// deferred while a recording or key is in progress.
pub struct LiveSettings {
    config: Mutex<Arc<Config>>,
    controller: ControllerHandle,
    hub: Mutex<Option<KeyboardHub>>,
    work: Option<Sender<WorkItem>>,
    menu: Option<MenuState>,
}

impl LiveSettings {
    pub fn new(
        config: Arc<Config>,
        controller: ControllerHandle,
        work: Option<Sender<WorkItem>>,
        menu: Option<MenuState>,
    ) -> Self {
        LiveSettings {
            config: Mutex::new(config),
            controller,
            hub: Mutex::new(None),
            work,
            menu,
        }
    }

    pub fn set_hub(&self, hub: KeyboardHub) {
        *self.hub.lock().unwrap_or_else(|p| p.into_inner()) = Some(hub);
    }

    pub fn config(&self) -> Arc<Config> {
        self.config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Apply a freshly loaded config; the names that changed.
    pub fn apply(&self, fresh: &Config) -> BTreeSet<String> {
        let mut current = self.config.lock().unwrap_or_else(|p| p.into_inner());
        let (mut merged, mut changed) = merge_live(&current, fresh, &runtime_fields());
        let groups = runtime_changes(&merged, fresh);
        if changed.is_empty() && groups.is_empty() {
            return changed;
        }
        for (group, fields) in RUNTIME_GROUPS {
            if groups.contains(group) {
                changed.extend(
                    fields
                        .iter()
                        .filter(|n| merged.field(n) != fresh.field(n))
                        .map(|n| n.to_string()),
                );
            }
        }
        merged = adopt_groups(&merged, fresh, &groups);
        let merged = Arc::new(merged);
        *current = merged.clone();
        drop(current);
        self.controller.update_config(merged.clone());
        if changed.contains(crate::settings::DICTATION_ENABLED) {
            if let Some(hub) = self.hub.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                hub.reconfigure_now(&keyboard_bindings(&merged, &self.controller));
            }
        } else if changed.iter().any(|n| HOTKEY_FIELDS.contains(&n.as_str())) {
            if let Some(hub) = self.hub.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                let (now, _) = hub.reconfigure(&keyboard_bindings(&merged, &self.controller));
                if !now {
                    log::info!("shortcut change waits until dictation is idle");
                }
            }
        }
        if let Some(work) = &self.work {
            if groups.contains("llm") {
                let _ = work.send(WorkItem::ReloadLlm(merged.clone()));
            }
            if groups.contains("stt") {
                let _ = work.send(WorkItem::ReloadStt(merged.clone()));
            }
        }
        if let Some(menu) = &self.menu {
            let enabled = dictation_enabled(&merged);
            menu.change(|m| m.dictation_enabled = enabled);
        }
        let names: Vec<&str> = changed.iter().map(String::as_str).collect();
        log::info!("settings updated without restart: {}", names.join(", "));
        changed
    }

    /// Retry a deferred shortcut rebind; true while one is still pending.
    pub fn retry(&self) -> bool {
        match self.hub.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            Some(hub) if hub.reconfigure_pending() => !hub.apply_pending(),
            _ => false,
        }
    }
}

/// A job channel sink for the controller (forwards into the worker queue).
pub fn job_sink(work: Sender<WorkItem>) -> crate::controller::JobSink {
    let work = Mutex::new(work);
    Arc::new(move |job: Job| {
        let _ = work
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(WorkItem::Job(job));
    })
}

pub fn work_queue() -> (Sender<WorkItem>, mpsc::Receiver<WorkItem>) {
    mpsc::channel()
}

/// The config file the runtime watches and toggles.
pub fn config_file() -> PathBuf {
    lf_core::config::config_path()
}
