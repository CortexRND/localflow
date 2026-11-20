//! Settings assistant: "Tell LocalFlow what to change".
//!
//! The state machine is `lf_ui::hub_settings::SettingsPageState` (busy /
//! preview / applied / error, tokens for stale answers, `STALE_PENDING` and
//! `STALE_PREVIEW`). The model is asked through `SettingsAgent::propose` on a
//! worker; Apply and Undo go through `settings_worker::dispatch` (worker
//! thread, `config_write` lock, pending-write registration). Nothing changes
//! before Apply. Undo is the agent's own: it skips (and reports) fields that
//! changed since, instead of overwriting newer edits.
//!
//! Real mode uses `NativeSettingsAgent::with_paths` on this window's explicit
//! config, dictionary and undo files (loopback Ollama only). The sandbox uses
//! the same native agent over the sandbox files with a scripted chat client,
//! so preview/apply/undo are real while no socket is opened.

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use anyhow::Result;
use lf_ui::backends::settings_agent::{Proposal, SettingsAgent};
use lf_ui::backends::settings_worker::{self, Completion, Outcome, Request};
use lf_ui::hub_settings::{SettingsPageState, Status};
use serde_json::{json, Value};

/// Mask anything that looks like a credential in text shown to the user.
/// Setting changes never carry keys (the allow-list has none), but model
/// output is untrusted, so every displayed line, note, warning, message,
/// undo summary and model name goes through this.
///
/// Spans are found independent of surrounding punctuation or whitespace
/// (newlines, JSON, URL queries): `name=value` / `"name": "value"` pairs for
/// credential-like names, `Bearer <token>`, provider-style prefixes
/// (`sk-…`, `gsk_…`, `ghp_…`, …) and long mixed letter/digit tokens.
pub fn mask_secrets(text: &str) -> String {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<[regex::Regex; 4]> = OnceLock::new();
    let [assignment, bearer, prefixed, long] = PATTERNS.get_or_init(|| {
        [
            regex::Regex::new(
                r#"(?i)((?:api[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token|token|secret|client[_-]?secret|password|passwd|apikey|private[_-]?key|secret[_-]?key)["']?\s*[:=]\s*["']?)([^\s"'&,;}\]]+)"#,
            )
            .expect("static regex"),
            regex::Regex::new(r"(?i)(bearer\s+)([A-Za-z0-9._~+/=\-]{8,})").expect("static regex"),
            regex::Regex::new(r"\b(?:sk|gsk|xai|fw|pk|rk|ghp|gho|hf|sk_live|sk_test)[-_][A-Za-z0-9_\-]{8,}")
                .expect("static regex"),
            regex::Regex::new(r"[A-Za-z0-9_\-]{24,}").expect("static regex"),
        ]
    });
    let mask = |value: &str| lf_providers::keys::mask_key(value);
    let text = assignment.replace_all(text, |c: &regex::Captures| {
        format!("{}{}", &c[1], mask(&c[2]))
    });
    let text = bearer.replace_all(&text, |c: &regex::Captures| {
        format!("{}{}", &c[1], mask(&c[2]))
    });
    let text = prefixed.replace_all(&text, |c: &regex::Captures| mask(&c[0]));
    long.replace_all(&text, |c: &regex::Captures| {
        let token = &c[0];
        let mixed = token.chars().any(|ch| ch.is_ascii_digit())
            && token.chars().any(|ch| ch.is_ascii_alphabetic());
        if mixed {
            mask(token)
        } else {
            token.to_string()
        }
    })
    .into_owned()
}

pub struct Assistant {
    pub state: SettingsPageState,
    agent: Arc<dyn SettingsAgent>,
    /// An Apply/Undo is in flight on the settings worker.
    pub writing: bool,
    /// Bumped by every Apply/Undo, so an older refresh can't overwrite the
    /// undo state a newer write produced.
    write_epoch: u64,
    /// Apply was refused by the agent (conflict, invalid value): shown as a
    /// danger banner while the preview stays.
    pub apply_error: Option<String>,
    pub expanded: bool,
    pub undo_skipped: Vec<String>,
}

impl Assistant {
    /// No I/O here (this runs on the UI thread): the undo summary is
    /// loaded by [`Assistant::begin_refresh`] on the settings worker.
    pub fn new(agent: Arc<dyn SettingsAgent>) -> Self {
        Assistant {
            state: SettingsPageState::new(),
            agent,
            writing: false,
            apply_error: None,
            expanded: false,
            undo_skipped: Vec::new(),
            write_epoch: 0,
        }
    }

    /// Load the read-only state (undo summary) with the canonical
    /// `Request::Refresh` on the settings worker; no preview or network.
    /// Returns the epoch to hand back to [`Assistant::refresh_done`].
    pub fn begin_refresh(&self) -> std::io::Result<(u64, mpsc::Receiver<Completion>)> {
        Ok((self.write_epoch, self.dispatch(Request::Refresh)?))
    }

    /// A refresh finished. Dropped when an Apply/Undo started since (its own
    /// completion carries the newer undo state) or is still running.
    pub fn refresh_done(&mut self, epoch: u64, completion: Completion) -> bool {
        if self.writing || epoch != self.write_epoch {
            return false;
        }
        self.state.undo_lines = completion.undo;
        true
    }

    pub fn agent(&self) -> Arc<dyn SettingsAgent> {
        self.agent.clone()
    }

    /// Preview pressed: `(token, request)` for the worker.
    pub fn start_preview(&mut self, request: &str) -> Option<(i64, String)> {
        if self.writing {
            return None;
        }
        let request: String = request
            .chars()
            .take(lf_ui::hub_settings::MAX_DRAFT_CHARS)
            .collect();
        let token = self.state.begin_preview(&request)?;
        self.apply_error = None;
        Some((token, request))
    }

    /// Blocking: ask the model (worker only).
    pub fn propose(
        agent: &dyn SettingsAgent,
        request: &str,
    ) -> (Result<Proposal, String>, Option<String>) {
        let result = agent.propose(request).map_err(|e| e.0);
        (result, agent.model())
    }

    pub fn preview_done(
        &mut self,
        token: i64,
        result: Result<Proposal, String>,
        model: Option<String>,
    ) -> bool {
        match result {
            Ok(proposal) => {
                self.state
                    .finish_preview(token, Some(proposal), None, model.as_deref())
            }
            Err(error) => self
                .state
                .finish_preview(token, None, Some(&error), model.as_deref()),
        }
    }

    /// Cancel: drops the in-flight answer by token; the request text stays.
    pub fn cancel(&mut self) {
        let draft = self.state.draft.clone();
        self.state.cancel();
        self.state.draft = draft;
        self.apply_error = None;
    }

    /// Settings changed under the assistant (manual row, external edit).
    pub fn settings_changed(&mut self) {
        self.state.settings_changed();
    }

    /// Apply pressed: the request for `settings_worker::dispatch`.
    pub fn begin_apply(&mut self) -> Option<Request> {
        if self.writing || self.state.status != Status::Preview {
            return None;
        }
        let proposal = self.state.proposal.as_ref()?;
        if proposal.is_empty() {
            return None;
        }
        self.writing = true;
        self.write_epoch += 1;
        self.apply_error = None;
        Some(Request::Apply(proposal.changes.clone()))
    }

    pub fn begin_undo(&mut self) -> Option<Request> {
        if self.writing || self.state.busy() || self.state.undo_lines.is_none() {
            return None;
        }
        self.writing = true;
        self.write_epoch += 1;
        Some(Request::Undo)
    }

    /// Dispatch a write on the settings worker; the completion arrives on
    /// the returned receiver.
    pub fn dispatch(&self, request: Request) -> std::io::Result<mpsc::Receiver<Completion>> {
        let (sender, receiver) = mpsc::channel();
        settings_worker::dispatch(self.agent.clone(), request, sender)?;
        Ok(receiver)
    }

    /// A write finished. Returns whether the config changed.
    pub fn completion(&mut self, completion: Completion) -> bool {
        self.writing = false;
        self.state.undo_lines = completion.undo;
        match completion.outcome {
            Outcome::Applied(Ok(result)) => {
                self.undo_skipped.clear();
                self.state.applied(&result);
                !result.applied.is_empty()
            }
            Outcome::Applied(Err(error)) => {
                // Keep the preview: the user can read it and decide.
                self.apply_error = Some(mask_secrets(
                    &lf_ui::backends::settings_agent::safe_text_default(&error.0),
                ));
                false
            }
            Outcome::Undone(Ok(result)) => {
                self.undo_skipped = result.skipped.iter().map(|s| mask_secrets(s)).collect();
                self.state.undone(&result);
                !result.reverted.is_empty()
            }
            Outcome::Undone(Err(error)) => {
                self.state.failed(&error.0);
                false
            }
            Outcome::Manual { .. } | Outcome::Refreshed => false,
        }
    }

    /// Display lines with secrets masked.
    pub fn lines(&self) -> Vec<String> {
        self.state.lines.iter().map(|l| mask_secrets(l)).collect()
    }
    pub fn notes(&self) -> Vec<String> {
        self.state.notes.iter().map(|l| mask_secrets(l)).collect()
    }
    pub fn warnings(&self) -> Vec<String> {
        self.state
            .warnings
            .iter()
            .map(|l| mask_secrets(l))
            .collect()
    }
    pub fn message(&self) -> String {
        mask_secrets(&self.state.message)
    }
}

/// The real agent on explicit paths (no default-path constructor).
pub fn native_agent(config: PathBuf, dictionary: PathBuf, undo: PathBuf) -> Arc<dyn SettingsAgent> {
    Arc::new(
        lf_ui::backends::settings_agent_native::NativeSettingsAgent::with_paths(
            config, dictionary, undo,
        ),
    )
}

/// The sandbox agent: the native agent over sandbox files with a scripted,
/// offline chat client.
pub fn sandbox_agent(
    config: PathBuf,
    dictionary: PathBuf,
    undo: PathBuf,
) -> Arc<dyn SettingsAgent> {
    Arc::new(
        lf_ui::backends::settings_agent_native::NativeSettingsAgent::with_client(
            SandboxChat,
            config,
            dictionary,
            undo,
        ),
    )
}

/// Deterministic answers for the sandbox; anything else is an error.
#[derive(Clone, Copy)]
pub struct SandboxChat;

pub const SANDBOX_MODEL: &str = "sandbox-model";
pub const SANDBOX_UNKNOWN: &str =
    "Sandbox: try “turn off sounds”, “casual tone”, “hold right control” or “add Kubernetes”.";

fn call(name: &str, arguments: Value) -> Value {
    json!({"function": {"name": name, "arguments": arguments}})
}

impl lf_agent::ChatClient for SandboxChat {
    fn tags(&self, _: &str) -> Result<Value> {
        Ok(json!({"models": [{"name": SANDBOX_MODEL}]}))
    }

    fn chat(&self, _: &str, payload: &Value) -> Result<Value> {
        let request = payload["messages"]
            .as_array()
            .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_lowercase();
        let calls = if request.contains("sound") {
            vec![call(
                "set_setting",
                json!({"key": "sounds_enabled", "value": false}),
            )]
        } else if request.contains("casual") {
            vec![call(
                "set_setting",
                json!({"key": "style_work", "value": "casual"}),
            )]
        } else if request.contains("right control") {
            vec![call(
                "set_setting",
                json!({"key": "hotkey", "value": "ctrl_r"}),
            )]
        } else if request.contains("kubernetes") {
            vec![call("add_dictionary_word", json!({"word": "Kubernetes"}))]
        } else {
            anyhow::bail!(SANDBOX_UNKNOWN);
        };
        Ok(json!({"message": {"content": "", "tool_calls": calls}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_ui::backends::settings_agent::{
        ApplyResult, Change, FakeCall, FakeSettingsAgent, SettingsAgentError, UndoResult,
    };
    use std::time::Duration;

    fn run(assistant: &mut Assistant, request: Request) -> bool {
        let rx = assistant.dispatch(request).unwrap();
        let completion = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assistant.completion(completion)
    }

    fn preview(assistant: &mut Assistant, text: &str) {
        let (token, request) = assistant.start_preview(text).unwrap();
        let (result, model) = Assistant::propose(assistant.agent().as_ref(), &request);
        assert!(assistant.preview_done(token, result, model));
    }

    #[test]
    fn preview_apply_undo_round_trip_writes_only_through_the_worker() {
        let fake = Arc::new(FakeSettingsAgent::new());
        let change = Change::setting("sounds_enabled", json!(true), json!(false));
        fake.script_proposal(Proposal::new(
            "turn off sounds",
            "m",
            vec![change.clone()],
            vec![],
        ));
        let mut assistant = Assistant::new(fake.clone());
        preview(&mut assistant, "turn off sounds");
        assert_eq!(assistant.state.status, Status::Preview);
        assert_eq!(assistant.lines().len(), 1);
        assert!(
            !fake.calls().iter().any(|c| matches!(c, FakeCall::Apply(_))),
            "nothing applied yet"
        );
        let request = assistant.begin_apply().unwrap();
        assert!(assistant.begin_apply().is_none(), "one write at a time");
        fake.script_apply(Ok(ApplyResult {
            applied: vec![change],
            message: "Saved.".into(),
        }));
        fake.set_undo_lines(Some(vec!["Sounds".into()]));
        assert!(run(&mut assistant, request));
        assert_eq!(assistant.state.status, Status::Applied);
        assert!(assistant.state.undo_lines.is_some());
        fake.script_undo(Ok(UndoResult {
            reverted: vec![],
            skipped: vec!["Sounds changed since; kept your newer value.".into()],
            message: "Nothing undone.".into(),
        }));
        let undo = assistant.begin_undo().unwrap();
        assert!(!run(&mut assistant, undo), "skipped undo changes nothing");
        assert_eq!(assistant.undo_skipped.len(), 1);
    }

    #[test]
    fn errors_conflicts_and_stale_answers_keep_the_request() {
        let fake = Arc::new(FakeSettingsAgent::new());
        fake.script_error(Some("m"), "Ollama isn't running.");
        let mut assistant = Assistant::new(fake.clone());
        preview(&mut assistant, "turn off sounds");
        assert_eq!(assistant.state.status, Status::Error);
        assert_eq!(assistant.state.draft, "turn off sounds");

        // Settings change while the model thinks: the answer is dropped.
        let change = Change::setting("sounds_enabled", json!(true), json!(false));
        fake.script_proposal(Proposal::new("x", "m", vec![change.clone()], vec![]));
        let (token, request) = assistant.start_preview("turn off sounds").unwrap();
        assistant.settings_changed();
        let (result, model) = Assistant::propose(fake.as_ref(), &request);
        assert!(!assistant.preview_done(token, result, model));
        assert_eq!(assistant.state.message, lf_ui::hub_settings::STALE_PENDING);

        // A preview made stale by a manual edit can't be applied.
        fake.script_proposal(Proposal::new("x", "m", vec![change.clone()], vec![]));
        preview(&mut assistant, "turn off sounds");
        assistant.settings_changed();
        assert_eq!(assistant.state.message, lf_ui::hub_settings::STALE_PREVIEW);
        assert!(assistant.begin_apply().is_none());

        // An apply the agent refuses keeps the preview and shows why.
        fake.script_proposal(Proposal::new("x", "m", vec![change], vec![]));
        preview(&mut assistant, "turn off sounds");
        fake.script_apply(Err(SettingsAgentError(
            "That shortcut is already used.".into(),
        )));
        let request = assistant.begin_apply().unwrap();
        run(&mut assistant, request);
        assert_eq!(assistant.state.status, Status::Preview);
        assert!(assistant
            .apply_error
            .as_deref()
            .unwrap()
            .contains("already used"));

        // Cancel drops the answer; the text stays.
        let (token, _) = assistant.start_preview("casual tone").unwrap();
        assistant.cancel();
        assert_eq!(assistant.state.draft, "casual tone");
        assert!(!assistant.preview_done(token, Err("late".into()), None));
    }

    #[test]
    fn construction_does_no_io_and_the_refresh_runs_on_the_worker() {
        use lf_ui::backends::settings_agent::{ManualSetting, SettingsAgentError};
        use std::sync::Mutex;
        struct Checked {
            inner: FakeSettingsAgent,
            owner: std::thread::ThreadId,
            calls: Mutex<Vec<(&'static str, bool)>>,
        }
        impl Checked {
            fn note(&self, name: &'static str) {
                let on_owner = std::thread::current().id() == self.owner;
                self.calls.lock().unwrap().push((name, on_owner));
            }
        }
        impl SettingsAgent for Checked {
            fn load(&self) -> Result<lf_core::config::Config, SettingsAgentError> {
                self.note("load");
                self.inner.load()
            }
            fn current_settings(
                &self,
                c: Option<&lf_core::config::Config>,
            ) -> Result<Vec<(String, String)>, SettingsAgentError> {
                self.note("current_settings");
                self.inner.current_settings(c)
            }
            fn manual_settings(
                &self,
                c: Option<&lf_core::config::Config>,
            ) -> Result<Vec<ManualSetting>, SettingsAgentError> {
                self.note("manual_settings");
                self.inner.manual_settings(c)
            }
            fn pending_undo(&self) -> Option<Vec<String>> {
                self.note("pending_undo");
                self.inner.pending_undo()
            }
            fn propose(&self, r: &str) -> Result<Proposal, SettingsAgentError> {
                self.note("propose");
                self.inner.propose(r)
            }
            fn set_setting(&self, k: &str, v: &Value) -> Result<ApplyResult, SettingsAgentError> {
                self.note("set_setting");
                self.inner.set_setting(k, v)
            }
            fn apply(&self, c: &[Change]) -> Result<ApplyResult, SettingsAgentError> {
                self.note("apply");
                self.inner.apply(c)
            }
            fn undo(&self) -> Result<UndoResult, SettingsAgentError> {
                self.note("undo");
                self.inner.undo()
            }
            fn model(&self) -> Option<String> {
                self.inner.model()
            }
        }
        let fake = FakeSettingsAgent::new();
        fake.set_undo_lines(Some(vec!["Sounds".into()]));
        let agent = Arc::new(Checked {
            inner: fake,
            owner: std::thread::current().id(),
            calls: Mutex::new(Vec::new()),
        });
        let mut assistant = Assistant::new(agent.clone());
        assert!(
            agent.calls.lock().unwrap().is_empty(),
            "no agent I/O while constructing"
        );
        assert!(assistant.state.undo_lines.is_none());
        let (epoch, rx) = assistant.begin_refresh().unwrap();
        let completion = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(assistant.refresh_done(epoch, completion));
        assert_eq!(assistant.state.undo_lines, Some(vec!["Sounds".to_string()]));
        let calls = agent.calls.lock().unwrap().clone();
        assert!(calls.iter().all(|(_, on_owner)| !on_owner), "{calls:?}");
        assert!(
            !calls.iter().any(|(n, _)| *n == "propose"),
            "no implicit preview"
        );

        // A refresh that loses the race with an Apply is dropped.
        let (stale_epoch, rx) = assistant.begin_refresh().unwrap();
        agent.inner.script_proposal(Proposal::new(
            "x",
            "m",
            vec![Change::setting("sounds_enabled", json!(true), json!(false))],
            vec![],
        ));
        preview(&mut assistant, "turn off sounds");
        let request = assistant.begin_apply().unwrap();
        let late = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!assistant.refresh_done(stale_epoch, late), "while writing");
        agent.inner.script_apply(Ok(ApplyResult {
            applied: vec![],
            message: "Saved.".into(),
        }));
        agent.inner.set_undo_lines(Some(vec!["Newer".into()]));
        run(&mut assistant, request);
        assert_eq!(assistant.state.undo_lines, Some(vec!["Newer".to_string()]));
        let (_, rx) = assistant.begin_refresh().unwrap();
        let fresh = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            !assistant.refresh_done(stale_epoch, fresh),
            "an epoch from before the Apply"
        );
    }

    #[test]
    fn secret_like_values_are_masked() {
        let cases = [
            "Skipped: api key sk-live-1234567890abcdefXYZ and token ghp_abcdefghijklmnop1234 kept",
            "credentials:\nsk-proj-AAAA1111BBBB2222",
            r#"{"api_key":"plainsecretvalue99"}"#,
            "https://host/v1?token=tok_live_ABCDEF123456&x=1",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig",
            "password: hunter2hunter2",
            "(gsk_0123456789abcdef)",
            "password: abc",
            "api_key=x",
        ];
        // Failure messages name the fixture by index only: the masker's
        // input and output are never printed, even when a redaction fails.
        for (index, (case, leak)) in cases
            .iter()
            .zip([
                "1234567890abcdef",
                "AAAA1111BBBB",
                "plainsecretval",
                "tok_live_ABCDEF12",
                "eyJhbGciOiJIUzI1NiJ9.payl",
                "hunter2hun",
                "0123456789abcd",
                "abc",
                "=x",
            ])
            .enumerate()
        {
            let masked = mask_secrets(case);
            assert!(
                !masked.contains(leak),
                "secret fixture #{index}: part of the secret survived masking"
            );
            assert!(
                masked.contains("••••"),
                "secret fixture #{index}: no mask marker in the output"
            );
        }
        // Ordinary preview text is untouched.
        for (index, plain) in [
            "Sounds: On → Off",
            "Add “Kubernetes” to the dictionary",
            "Push-to-talk shortcut: Right Option → Right Control",
            "pause_media_while_dictating: Off → On",
            "Command key: Right Option → Left Option",
            "Dictation shortcut: Hold F19",
            "Runs on this computer with accounts/fireworks/models/kimi-k3",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                mask_secrets(plain) == plain,
                "plain fixture #{index} was changed by masking"
            );
        }
    }

    #[test]
    fn sandbox_agent_previews_applies_and_undoes_against_sandbox_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::data::DataRoot::sandbox(dir.path());
        lf_core::config::save_config(
            &lf_core::config::Config::defaults_for(false),
            Some(&root.config_path),
        )
        .unwrap();
        let agent = sandbox_agent(
            root.config_path.clone(),
            root.dictionary_path.clone(),
            root.localflow_dir.join("settings-undo.json"),
        );
        let mut assistant = Assistant::new(agent);
        preview(&mut assistant, "please turn off sounds");
        // Diagnostics name the status only: assistant messages can quote the
        // request or an error, which may carry sensitive text.
        assert_eq!(
            assistant.state.status,
            Status::Preview,
            "the sandbox agent didn't produce a preview"
        );
        let request = assistant.begin_apply().unwrap();
        assert!(run(&mut assistant, request));
        let saved = lf_core::config::load_from(&root.config_path).unwrap();
        assert_eq!(saved.field_bool("sounds_enabled"), Some(false));
        let undo = assistant.begin_undo().unwrap();
        assert!(run(&mut assistant, undo));
        let restored = lf_core::config::load_from(&root.config_path).unwrap();
        assert_eq!(restored.field_bool("sounds_enabled"), Some(true));
        preview(&mut assistant, "make it purple");
        assert_eq!(assistant.state.status, Status::Error);
        assert!(
            assistant.message().contains("Sandbox"),
            "the refusal message doesn't mention the sandbox"
        );
        for path in [&root.config_path, &root.dictionary_path] {
            assert!(path.starts_with(dir.path()));
        }
    }
}
