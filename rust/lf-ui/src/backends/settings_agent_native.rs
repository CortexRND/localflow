//! Real [`SettingsAgent`] adapter backed by `lf-agent`.
//!
//! `lf-agent` intentionally uses typed changes and `anyhow` errors, whereas the
//! Hub's presentation contract carries display-bearing changes and friendly
//! string errors. This module is only that translation boundary: validation,
//! stale checks, Ollama access, config/dictionary writes and undo bookkeeping
//! all stay in `lf-agent`. There is no in-memory production fallback.
//!
//! The upstream agent's injectable clock is not `Send`/`Sync`. A native agent is
//! therefore created inside each operation and dropped on that same thread. The
//! Hub calls `propose` on its preview worker; local reads and writes never wait
//! behind a model request. No unsafe auto-trait implementation is involved.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lf_agent::{ChatClient, OllamaClient};
use lf_core::config::{try_config_path, Config};
use lf_core::dictionary::dictionary_path;
use serde_json::Value;

use super::settings_agent::{
    label, note, ApplyResult, Change, ManualSetting, Proposal, SettingsAgent, SettingsAgentError,
    UndoResult, KIND_REPLACEMENT, KIND_SETTING, KIND_WORD,
};

fn user_error(error: impl std::fmt::Display) -> SettingsAgentError {
    SettingsAgentError(error.to_string())
}

fn change_to_ui(change: &lf_agent::Change) -> Change {
    match change {
        lf_agent::Change::Setting { key, before, value } => Change {
            // Keep native display values as well as raw values. The native page
            // uses these for the two dictation keys newer than the UI allowlist.
            label: label(key)
                .unwrap_or_else(|| lf_agent::label(key))
                .to_string(),
            old: lf_agent::display_value(key, before),
            new: lf_agent::display_value(key, value),
            kind: KIND_SETTING.to_string(),
            key: key.clone(),
            before: before.clone(),
            value: value.clone(),
            note: note(key).to_string(),
        },
        lf_agent::Change::Word(word) => Change::word(word),
        lf_agent::Change::Replacement { spoken, written } => Change::replacement(spoken, written),
    }
}

fn invalid_change() -> SettingsAgentError {
    SettingsAgentError("A change that isn't valid (preview again)".into())
}

fn change_from_ui(change: &Change) -> Result<lf_agent::Change, SettingsAgentError> {
    match change.kind.as_str() {
        KIND_SETTING => Ok(lf_agent::Change::Setting {
            key: change.key.clone(),
            before: change.before.clone(),
            value: change.value.clone(),
        }),
        KIND_WORD => change
            .value
            .as_str()
            .map(|word| lf_agent::Change::Word(word.to_string()))
            .ok_or_else(invalid_change),
        KIND_REPLACEMENT => change
            .replacement_pair()
            .map(|(spoken, written)| lf_agent::Change::Replacement {
                spoken: spoken.to_string(),
                written: written.to_string(),
            })
            .ok_or_else(invalid_change),
        _ => Err(invalid_change()),
    }
}

fn apply_to_ui(result: lf_agent::ApplyResult) -> ApplyResult {
    ApplyResult {
        applied: result.changes.iter().map(change_to_ui).collect(),
        message: result.message,
    }
}

type AgentFactory<C> =
    Arc<dyn Fn() -> Result<lf_agent::SettingsAgent<C>, SettingsAgentError> + Send + Sync + 'static>;

/// Thread-safe factory adapter for the real `lf-agent` implementation.
///
/// Construct with [`Self::with_client`] and injected storage/client in tests, or
/// use [`NativeSettingsAgent::<OllamaClient>::with_paths`] /
/// [`native_settings_agent`] for real loopback Ollama. The config, dictionary,
/// validator and undo state live in their normal files, not in this adapter.
pub struct NativeSettingsAgent<C: ChatClient + 'static> {
    factory: AgentFactory<C>,
    // A successful Proposal contains the chosen model. A newer failed request
    // clears it instead of accidentally displaying the previous model.
    model: Arc<Mutex<Option<String>>>,
}

impl<C> NativeSettingsAgent<C>
where
    C: ChatClient + 'static,
{
    fn from_factory(factory: AgentFactory<C>) -> Self {
        Self {
            factory,
            model: Arc::new(Mutex::new(None)),
        }
    }

    /// Use a clone of an injected client in each operation. Clones never leave
    /// the thread on which their operation runs; no network request is made by
    /// this constructor.
    pub fn with_client(
        client: C,
        config_path: PathBuf,
        dictionary_path: PathBuf,
        undo_path: PathBuf,
    ) -> Self
    where
        C: Clone + Send + Sync,
    {
        Self::from_factory(Arc::new(move || {
            Ok(lf_agent::SettingsAgent::new(
                client.clone(),
                config_path.clone(),
                dictionary_path.clone(),
                undo_path.clone(),
            ))
        }))
    }

    fn agent(&self) -> Result<lf_agent::SettingsAgent<C>, SettingsAgentError> {
        (self.factory)()
    }
}

impl NativeSettingsAgent<OllamaClient> {
    /// A real agent over explicit storage paths. The Ollama HTTP client is
    /// constructed for an operation; no network request is made here.
    pub fn with_paths(config_path: PathBuf, dictionary_path: PathBuf, undo_path: PathBuf) -> Self {
        Self::from_factory(Arc::new(move || {
            let client = OllamaClient::new().map_err(user_error)?;
            Ok(lf_agent::SettingsAgent::new(
                client,
                config_path.clone(),
                dictionary_path.clone(),
                undo_path.clone(),
            ))
        }))
    }

    /// A real agent using LocalFlow's normal config, dictionary and undo paths.
    pub fn for_default_paths() -> Result<Self, SettingsAgentError> {
        Ok(Self::with_paths(
            try_config_path().map_err(user_error)?,
            dictionary_path(),
            lf_agent::default_undo_path().map_err(user_error)?,
        ))
    }
}

/// The production factory used by the Hub's service wiring.
pub fn native_settings_agent() -> Result<Box<dyn SettingsAgent>, SettingsAgentError> {
    Ok(Box::new(NativeSettingsAgent::for_default_paths()?))
}

impl<C> SettingsAgent for NativeSettingsAgent<C>
where
    C: ChatClient + 'static,
{
    fn load(&self) -> Result<Config, SettingsAgentError> {
        self.agent()?.load().map_err(user_error)
    }

    fn current_settings(
        &self,
        _config: Option<&Config>,
    ) -> Result<Vec<(String, String)>, SettingsAgentError> {
        // Native reads deliberately reload instead of presenting a stale
        // optional caller snapshot as the current on-disk settings.
        self.agent()?.current_settings().map_err(user_error)
    }

    fn manual_settings(
        &self,
        _config: Option<&Config>,
    ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
        self.agent()?
            .manual_settings()
            .map(|rows| {
                rows.into_iter()
                    .map(|row| ManualSetting {
                        key: row.key,
                        group: row.group,
                        label: row.label,
                        kind: row.kind,
                        value: row.valid.then_some(row.value),
                        valid: row.valid,
                        display: row.display,
                        choices: row.choices,
                    })
                    .collect()
            })
            .map_err(user_error)
    }

    fn pending_undo(&self) -> Option<Vec<String>> {
        self.agent().ok()?.pending_undo()
    }

    fn propose(&self, request: &str) -> Result<Proposal, SettingsAgentError> {
        *self
            .model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let proposal = self.agent()?.propose(request).map_err(user_error)?;
        *self
            .model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(proposal.model.clone());
        Ok(Proposal::new(
            &proposal.request,
            &proposal.model,
            proposal.changes.iter().map(change_to_ui).collect(),
            proposal.rejected,
        ))
    }

    fn set_setting(&self, key: &str, value: &Value) -> Result<ApplyResult, SettingsAgentError> {
        crate::config_write::with_lock(|| {
            self.agent()?
                .set_setting(key, value.clone())
                .map(apply_to_ui)
                .map_err(user_error)
        })
    }

    fn apply(&self, changes: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
        let native = changes
            .iter()
            .map(change_from_ui)
            .collect::<Result<Vec<_>, _>>()?;
        crate::config_write::with_lock(|| {
            self.agent()?
                .apply(&native)
                .map(apply_to_ui)
                .map_err(user_error)
        })
    }

    fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
        crate::config_write::with_lock(|| {
            self.agent()?
                .undo()
                .map(|result| UndoResult {
                    reverted: result.lines,
                    skipped: result.skipped,
                    message: result.message,
                })
                .map_err(user_error)
        })
    }

    fn model(&self) -> Option<String> {
        self.model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use lf_agent::ChatClient;
    use lf_core::config::{load_from, save_config, Config};
    use serde_json::{json, Value};

    use super::*;

    #[derive(Clone)]
    struct TestChat {
        chat: Value,
    }

    impl ChatClient for TestChat {
        fn tags(&self, endpoint: &str) -> Result<Value> {
            assert_eq!(endpoint, "http://localhost:11434");
            Ok(json!({"models": [{"name": "gemma4:e4b"}]}))
        }

        fn chat(&self, endpoint: &str, payload: &Value) -> Result<Value> {
            assert_eq!(endpoint, "http://localhost:11434");
            assert_eq!(payload["stream"], false);
            assert_eq!(payload["model"], "gemma4:e4b");
            Ok(self.chat.clone())
        }
    }

    fn call(name: &str, arguments: Value) -> Value {
        json!({"function": {"name": name, "arguments": arguments}})
    }

    fn setup(calls: Vec<Value>) -> (tempfile::TempDir, NativeSettingsAgent<TestChat>) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        save_config(&Config::defaults_for(false), Some(&config_path)).unwrap();
        let chat = json!({"message": {"content": "", "tool_calls": calls}});
        let agent = NativeSettingsAgent::with_client(
            TestChat { chat },
            config_path,
            dir.path().join("dictionary.json"),
            dir.path().join("undo.json"),
        );
        (dir, agent)
    }

    #[test]
    fn adapter_is_send_sync() {
        fn assert_agent<T: SettingsAgent>() {}
        assert_agent::<NativeSettingsAgent<TestChat>>();
    }

    #[test]
    fn concurrent_agent_and_home_changes_survive() {
        use std::sync::{mpsc, Condvar};
        use std::time::Duration;

        let (dir, _) = setup(Vec::new());
        let path = dir.path().join("config.toml");
        let (loaded, loading) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let adapter = {
            let path = path.clone();
            let root = dir.path().to_path_buf();
            let release = release.clone();
            NativeSettingsAgent::from_factory(Arc::new(move || {
                let mut agent = lf_agent::SettingsAgent::new(
                    TestChat { chat: json!({}) },
                    path.clone(),
                    root.join("dictionary.json"),
                    root.join("undo.json"),
                );
                let release = release.clone();
                let loaded = loaded.clone();
                agent.clock = Box::new(move || {
                    loaded.send(()).unwrap();
                    let (lock, wake) = &*release;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        let (next, timeout) =
                            wake.wait_timeout(ready, Duration::from_secs(5)).unwrap();
                        ready = next;
                        assert!(!timeout.timed_out(), "agent write was never released");
                    }
                    1.0
                });
                Ok(agent)
            }))
        };
        let (sent, received) = mpsc::channel();
        let worker = crate::backends::settings_worker::dispatch(
            Arc::new(adapter),
            crate::backends::settings_worker::Request::Manual {
                key: "sounds_enabled".into(),
                value: json!(false),
                typed: None,
            },
            sent,
        )
        .unwrap();
        loading.recv_timeout(Duration::from_secs(5)).unwrap();
        let (home_starting, home_started) = mpsc::channel();
        let (home_finished, home_done) = mpsc::channel();
        let home = {
            let path = path.clone();
            std::thread::spawn(move || {
                home_starting.send(()).unwrap();
                crate::config_write::update(Some(&path), |latest| {
                    lf_core::config::apply_config_update(
                        latest,
                        &json!({"dictation": {"enabled": false}}),
                    )
                })
                .unwrap();
                home_finished.send(()).unwrap();
            })
        };
        home_started.recv_timeout(Duration::from_secs(5)).unwrap();
        let home_waited = home_done.recv_timeout(Duration::from_millis(100)).is_err();
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        let completion = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            completion.outcome,
            crate::backends::settings_worker::Outcome::Manual { result: Ok(_), .. }
        ));
        worker.join().unwrap();
        home.join().unwrap();
        assert!(
            home_waited,
            "Home wrote while the agent held a read/modify/write transaction"
        );
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
    }

    #[test]
    fn an_agent_write_waits_for_a_home_transaction() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (dir, adapter) = setup(Vec::new());
        let path = dir.path().join("config.toml");
        let (loaded, loading) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let home = {
            let path = path.clone();
            std::thread::spawn(move || {
                crate::config_write::update(Some(&path), |latest| {
                    loaded.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                    lf_core::config::apply_config_update(
                        latest,
                        &json!({"dictation": {"enabled": false}}),
                    )
                })
                .unwrap();
            })
        };
        loading.recv_timeout(Duration::from_secs(5)).unwrap();
        let (starting, started) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            starting.send(()).unwrap();
            let result = adapter.set_setting("sounds_enabled", &json!(false));
            finished.send(result).unwrap();
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let agent_waited = done.recv_timeout(Duration::from_millis(100)).is_err();
        release.send(()).unwrap();
        home.join().unwrap();
        let result = done.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(
            agent_waited,
            "agent wrote while Home held a read/modify/write transaction"
        );
        result.unwrap();
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
    }

    #[test]
    fn manual_write_reloads_config_and_preserves_external_changes() {
        let (dir, adapter) = setup(Vec::new());
        let path = dir.path().join("config.toml");
        let _stale = adapter.load().unwrap();
        let mut external = lf_core::config::load_from(&path).unwrap();
        external.set_field("dictation_enabled", json!(false));
        save_config(&external, Some(&path)).unwrap();
        adapter
            .set_setting("sounds_enabled", &json!(false))
            .unwrap();
        let saved = lf_core::config::load_from(&path).unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
    }

    #[test]
    fn a_preview_does_not_block_manual_writes_on_the_ui_thread() {
        use std::sync::mpsc;
        use std::time::Duration;

        #[derive(Clone)]
        struct SlowChat {
            release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        }
        impl ChatClient for SlowChat {
            fn tags(&self, _: &str) -> Result<Value> {
                Ok(json!({"models": [{"name": "m"}]}))
            }
            fn chat(&self, _: &str, _: &Value) -> Result<Value> {
                let (lock, wake) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                Ok(json!({"message": {"tool_calls": [call(
                    "set_setting",
                    json!({"key": "sounds_enabled", "value": "off"}),
                )]}}))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        save_config(&Config::defaults_for(false), Some(&config_path)).unwrap();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let adapter = Arc::new(NativeSettingsAgent::with_client(
            SlowChat {
                release: Arc::clone(&release),
            },
            config_path.clone(),
            dir.path().join("dictionary.json"),
            dir.path().join("undo.json"),
        ));
        let worker = {
            let adapter = Arc::clone(&adapter);
            std::thread::spawn(move || adapter.propose("turn off sounds"))
        };
        let (done, finished) = mpsc::channel();
        let writer = {
            let adapter = Arc::clone(&adapter);
            std::thread::spawn(move || {
                let result = adapter.set_setting("cleanup_enabled", &json!(false));
                let _ = done.send(result.is_ok());
            })
        };
        let write_finished = finished.recv_timeout(Duration::from_secs(2));
        // Always release the preview before assertions/joins so a failed check
        // cannot leave the test process with a permanently parked worker.
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        worker.join().unwrap().unwrap();
        writer.join().unwrap();
        assert_eq!(write_finished, Ok(true));
    }

    #[test]
    fn manual_and_current_rows_are_native_agent_data() {
        let (_dir, adapter) = setup(vec![]);
        let rows = adapter.manual_settings(None).unwrap();
        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0].key, "dictation_enabled");
        assert_eq!(rows[0].label, "Dictation");
        assert_eq!(rows[0].kind, "switch");
        assert_eq!(rows[0].value, Some(json!(true)));
        assert_eq!(rows[6].key, "cleanup_enabled");
        let toggle = rows
            .iter()
            .find(|row| row.key == "dictation_toggle_hotkey")
            .unwrap();
        assert_eq!(toggle.kind, "shortcut");
        assert_eq!(toggle.label, "Toggle dictation shortcut");
        let current = adapter.current_settings(None).unwrap();
        assert_eq!(current[0], ("Sounds".into(), "On".into()));
    }

    #[test]
    fn ui_display_tables_match_lf_agent() {
        use crate::backends::settings_agent::{
            display_value, ALLOWLIST, BOOL_KEYS, MANUAL_GROUPS, SHORTCUT_KEYS,
        };

        assert_eq!(ALLOWLIST.as_slice(), lf_agent::ALLOWLIST);
        for key in lf_agent::ALLOWLIST {
            assert_eq!(label(key), Some(lf_agent::label(key)), "{key}");
        }
        assert_eq!(MANUAL_GROUPS.as_slice(), lf_agent::MANUAL_GROUPS);
        assert_eq!(BOOL_KEYS.as_slice(), lf_agent::BOOL_KEYS);
        let samples = [
            json!(true),
            json!(false),
            json!(""),
            json!("alt_r"),
            json!("<ctrl>+<alt>+d"),
            json!("osascript"),
            json!("very_casual"),
        ];
        for key in lf_agent::ALLOWLIST {
            for value in &samples {
                assert_eq!(
                    display_value(key, value),
                    lf_agent::display_value(key, value),
                    "{key} = {value}"
                );
            }
        }
        assert!(SHORTCUT_KEYS.iter().all(|k| ALLOWLIST.contains(k)));

        // Every key `normalize_combo` accepts (its named-key list in
        // lf-agent's private `validation.rs`, f1–f20, single characters) and
        // every modifier / hold key, alone and in combos.
        let named = [
            "space",
            "tab",
            "enter",
            "backspace",
            "delete",
            "home",
            "end",
            "page_up",
            "page_down",
            "up",
            "down",
            "left",
            "right",
            "insert",
        ];
        let modifiers = [
            "alt", "alt_l", "alt_r", "alt_gr", "ctrl", "ctrl_l", "ctrl_r", "cmd", "cmd_l", "cmd_r",
            "shift", "shift_l", "shift_r",
        ];
        let mut keys: Vec<String> = named.iter().map(|k| k.to_string()).collect();
        keys.extend((1..=20).map(|n| format!("f{n}")));
        keys.extend(["d", "1", "/", "caps_lock"].map(String::from));
        keys.extend(modifiers.iter().map(|m| m.to_string()));
        let mut shortcuts = Vec::new();
        for key in &keys {
            shortcuts.push(key.clone());
            shortcuts.push(format!("<ctrl>+{key}"));
            shortcuts.push(format!("<cmd>+<shift>+{key}"));
        }
        shortcuts.extend(modifiers.iter().map(|m| format!("<{m}>+<alt>+1")));
        for shortcut in &shortcuts {
            assert_eq!(
                super::super::settings_agent::format_shortcut(shortcut),
                lf_agent::format_shortcut(shortcut),
                "{shortcut}"
            );
            for key in SHORTCUT_KEYS {
                let value = json!(shortcut);
                assert_eq!(
                    display_value(key, &value),
                    lf_agent::display_value(key, &value),
                    "{key} = {shortcut}"
                );
            }
        }
    }

    #[test]
    fn a_real_toggle_shortcut_preview_has_a_shortcut_line() {
        let (_dir, adapter) = setup(vec![call(
            "set_setting",
            json!({"key": "dictation_toggle_hotkey", "value": "ctrl+alt+d"}),
        )]);
        let proposal = adapter
            .propose("set the toggle dictation shortcut to ctrl+alt+d")
            .unwrap();
        assert_eq!(proposal.changes.len(), 1, "{:?}", proposal.rejected);
        let change = &proposal.changes[0];
        assert_eq!(
            change.line(),
            format!("Toggle dictation shortcut: {} → {}", change.old, change.new)
        );
        assert_eq!(
            change.warning(&proposal.request),
            "Check: changes a keyboard shortcut."
        );
    }

    #[test]
    fn a_real_preview_translates_and_apply_writes_through_lf_agent() {
        let (_dir, adapter) = setup(vec![call(
            "set_setting",
            json!({"key": "dictation_enabled", "value": "off"}),
        )]);
        let proposal = adapter
            .propose("turn dictation off with dictation_enabled")
            .unwrap();
        assert_eq!(adapter.model().as_deref(), Some("gemma4:e4b"));
        assert_eq!(proposal.changes.len(), 1);
        let change = &proposal.changes[0];
        assert_eq!(change.key, "dictation_enabled");
        assert_eq!(change.label, "Dictation");
        assert_eq!(change.old, "On");
        assert_eq!(change.new, "Off");
        assert_eq!(change.kind, KIND_SETTING);
        assert_eq!(change.line(), "Dictation: On → Off");
        assert_eq!(
            change.warning(&proposal.request),
            "Check: turns Dictation off."
        );
        let applied = adapter.apply(&proposal.changes).unwrap();
        assert_eq!(applied.applied.len(), 1);
        assert_eq!(applied.applied[0].label, "Dictation");
        assert_eq!(applied.applied[0].line(), "Dictation: On → Off");
        assert_eq!(applied.message, "Applied 1 change.");
        assert_eq!(
            adapter.load().unwrap().field_bool("dictation_enabled"),
            Some(false)
        );
        assert_eq!(adapter.pending_undo().unwrap().len(), 1);
    }

    #[test]
    fn real_apply_immediately_exposes_model_and_undo_summary() {
        let (dir, adapter) = setup(vec![
            call(
                "set_setting",
                json!({"key": "sounds_enabled", "value": false}),
            ),
            call("set_setting", json!({"key": "hotkey", "value": "ctrl_r"})),
            call("add_dictionary_word", json!({"word": "Kubernetes"})),
        ]);
        assert_eq!(adapter.pending_undo(), None);
        let proposal = adapter
            .propose("Turn off sounds, dictate with Right Ctrl, learn Kubernetes")
            .unwrap();
        assert_eq!(proposal.model, "gemma4:e4b");

        let applied = adapter.apply(&proposal.changes).unwrap();
        assert_eq!(applied.message, "Applied 3 changes.");
        assert_eq!(adapter.model().as_deref(), Some("gemma4:e4b"));
        let expected = Some(vec![
            "Sounds: Off → On".into(),
            "Dictation shortcut: Hold Right Ctrl → Hold Left Option".into(),
            "Remove word “Kubernetes”".into(),
        ]);
        // This call creates a new real lf-agent over the same isolated paths,
        // just as refresh_local does during the rebuild immediately after Apply.
        assert_eq!(adapter.pending_undo(), expected);
        assert!(dir.path().join("undo.json").is_file());

        let undone = adapter.undo().unwrap();
        assert_eq!(undone.reverted.len(), 3);
        assert_eq!(adapter.pending_undo(), None);
        assert_eq!(adapter.model().as_deref(), Some("gemma4:e4b"));
    }

    #[test]
    fn preview_has_no_writes_and_round_trips_dictionary_changes() {
        let (dir, adapter) = setup(vec![
            call("add_dictionary_word", json!({"word": "Kubernetes"})),
            call(
                "add_replacement",
                json!({"spoken": "local flow", "written": "LocalFlow"}),
            ),
        ]);
        let proposal = adapter
            .propose("add Kubernetes and replace local flow with LocalFlow")
            .unwrap();
        assert_eq!(proposal.changes.len(), 2);
        assert!(!dir.path().join("dictionary.json").exists());
        let applied = adapter.apply(&proposal.changes).unwrap();
        assert_eq!(applied.applied.len(), 2);
        assert_eq!(applied.applied[0].kind, KIND_WORD);
        assert_eq!(applied.applied[1].kind, KIND_REPLACEMENT);
        assert_eq!(adapter.pending_undo().unwrap().len(), 2);
        let undone = adapter.undo().unwrap();
        assert_eq!(undone.reverted.len(), 2);
    }

    #[test]
    fn manual_save_and_undo_stay_in_native_storage() {
        let (dir, adapter) = setup(vec![]);
        let result = adapter
            .set_setting("sounds_enabled", &json!(false))
            .unwrap();
        assert_eq!(result.applied[0].line(), "Sounds: On → Off");
        assert_eq!(
            load_from(&dir.path().join("config.toml"))
                .unwrap()
                .field_bool("sounds_enabled"),
            Some(false)
        );
        let undone = adapter.undo().unwrap();
        assert_eq!(undone.reverted, ["Sounds: On"]);
        assert_eq!(
            adapter.load().unwrap().field_bool("sounds_enabled"),
            Some(true)
        );
        assert!(adapter.pending_undo().is_none());
    }

    #[test]
    fn errors_are_user_facing_and_a_new_request_clears_the_old_model() {
        let (_dir, adapter) = setup(vec![call("add_dictionary_word", json!({"word": "   "}))]);
        assert!(adapter.propose("add a word").is_err());
        assert_eq!(adapter.model(), None);
        assert!(adapter
            .apply(&[Change::setting("sounds_enabled", json!(true), Value::Null,)])
            .is_err());
        let mut not_text = Change::word("x");
        not_text.value = json!(5);
        assert!(adapter.apply(&[not_text]).is_err());
    }
}
