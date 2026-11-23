//! `tweak`: the Python `lf tweak` (`localflow/cli.py` `tweak`) over the
//! `lf-agent` settings agent. The preview is printed in full, even with
//! `--yes`; every line from the model, Ollama, the config or the undo record is
//! escaped with `safe_text`, as Python does.

use anyhow::{anyhow, Result};
use lf_agent::{safe_text, ChatClient, OllamaClient, SettingsAgent};
use lf_core::{config, dictionary};

use crate::args::TweakArgs;
use crate::pyfmt::py_strip;
use crate::{confirm, outln, usage, CliError, Io};

/// `settings_agent.MAX_DISPLAY_CHARS`, safe_text's default limit.
const MAX_DISPLAY_CHARS: usize = 300;

pub(crate) fn run(args: TweakArgs, io: &mut Io<'_>) -> Result<()> {
    run_with(args, io, || {
        // Python's agent reads through `load_config()`, which first migrates a
        // legacy `~/.localflow.toml` into the v2 file this agent then reads.
        config::load_config().map_err(|e| anyhow!("Couldn't read your settings ({e})."))?;
        Ok(SettingsAgent::new(
            OllamaClient::new()?,
            config::try_config_path()?,
            dictionary::dictionary_path(),
            lf_agent::default_undo_path()?,
        ))
    })
}

/// `cli.tweak` with the agent from `agent` (the `_tweak_agent` seam).
pub(crate) fn run_with<C: ChatClient>(
    args: TweakArgs,
    io: &mut Io<'_>,
    agent: impl FnOnce() -> Result<SettingsAgent<C>>,
) -> Result<()> {
    if args.undo {
        if args.request.is_some() {
            return usage("use either a request or --undo, not both");
        }
        if args.yes {
            return usage("--yes only applies to a request");
        }
        let result = agent().and_then(|agent| agent.undo()).map_err(failed)?;
        for line in &result.lines {
            outln!(io.out, "  {}", safe_text(line, MAX_DISPLAY_CHARS))?;
        }
        return outln!(io.out, "{}", safe_text(&result.message, MAX_DISPLAY_CHARS));
    }
    let Some(request) = args.request.filter(|r| !py_strip(r).is_empty()) else {
        return usage("say what to change, e.g. lf tweak \"turn off sounds\"");
    };
    let agent = agent().map_err(failed)?;
    let proposal = agent.propose(&request).map_err(failed)?;
    outln!(
        io.out,
        "Preview (runs on your computer with {}):",
        safe_text(&proposal.model, 120)
    )?;
    for change in &proposal.changes {
        outln!(io.out, "  {}", change.line())?;
    }
    for warning in proposal.warnings() {
        outln!(io.out, "  WARNING: {warning}")?;
    }
    for note in &proposal.rejected {
        outln!(io.out, "  skipped: {}", safe_text(note, MAX_DISPLAY_CHARS))?;
    }
    if !args.yes && !confirm("Apply these changes?", io)? {
        return outln!(io.out, "Nothing changed.");
    }
    let result = agent.apply(&proposal.changes).map_err(failed)?;
    outln!(io.out, "{}", safe_text(&result.message, MAX_DISPLAY_CHARS))?;
    if !result.changes.is_empty() {
        outln!(io.out, "Undo with: lf tweak --undo")?;
    }
    Ok(())
}

/// `click.ClickException(safe_text(str(exc)))`.
fn failed(error: anyhow::Error) -> anyhow::Error {
    CliError::Failed(safe_text(&error.to_string(), MAX_DISPLAY_CHARS)).into()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use anyhow::bail;
    use lf_core::dictionary::DictionaryStore;
    use serde_json::{json, Value};

    use super::*;
    use crate::report;
    use crate::testing::{run_cli, FakeClipboard};

    /// `tests/test_settings_agent.FakeOllama`: one installed model and a fixed
    /// list of tool calls, or a connection error.
    #[derive(Clone, Default)]
    struct FakeOllama {
        calls: Vec<Value>,
        unreachable: bool,
        requests: Rc<RefCell<Vec<Value>>>,
    }

    impl ChatClient for FakeOllama {
        fn tags(&self, endpoint: &str) -> Result<Value> {
            if self.unreachable {
                bail!("Couldn't reach Ollama at {endpoint}. Start Ollama, then try again.");
            }
            Ok(json!({"models": [{"name": "gemma4:e4b"}]}))
        }

        fn chat(&self, _endpoint: &str, payload: &Value) -> Result<Value> {
            self.requests.borrow_mut().push(payload.clone());
            Ok(json!({"message": {"tool_calls": self.calls}}))
        }
    }

    fn call(name: &str, arguments: Value) -> Value {
        json!({"function": {"name": name, "arguments": arguments}})
    }

    struct Env {
        dir: tempfile::TempDir,
        ollama: FakeOllama,
        /// How many agents the command asked for (`tweak_env["sessions"]`).
        agents: RefCell<usize>,
    }

    struct Outcome {
        code: i32,
        out: String,
        err: String,
    }

    impl Env {
        fn new() -> Self {
            let ollama = FakeOllama {
                calls: vec![
                    call(
                        "set_setting",
                        json!({"key": "sounds_enabled", "value": "off"}),
                    ),
                    call("add_dictionary_word", json!({"word": "Kubernetes"})),
                ],
                ..FakeOllama::default()
            };
            Self {
                dir: tempfile::tempdir().unwrap(),
                ollama,
                agents: RefCell::new(0),
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn run(&self, args: &[&str], input: &str) -> Outcome {
            let mut argv = vec!["lf-rs", "tweak"];
            argv.extend_from_slice(args);
            let parsed = match <crate::args::Cli as clap::Parser>::try_parse_from(&argv) {
                Ok(cli) => match cli.command {
                    crate::args::Command::Tweak(args) => args,
                    _ => unreachable!(),
                },
                // Parse errors never reach the agent; `run_cli` covers them.
                Err(_) => {
                    let outcome = run_cli(&argv[1..]);
                    return Outcome {
                        code: outcome.code,
                        out: outcome.out,
                        err: outcome.err,
                    };
                }
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut reader = Cursor::new(input.as_bytes().to_vec());
            let mut clipboard = FakeClipboard::default();
            let mut io = Io {
                out: &mut out,
                err: &mut err,
                input: &mut reader,
                clipboard: &mut clipboard,
                server_url: None,
            };
            let result = run_with(parsed, &mut io, || {
                *self.agents.borrow_mut() += 1;
                Ok(SettingsAgent::new(
                    self.ollama.clone(),
                    self.path("config.toml"),
                    self.path("dictionary.json"),
                    self.path("settings-undo.json"),
                ))
            });
            let code = report(result, &mut io);
            Outcome {
                code,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            }
        }

        fn sounds(&self) -> bool {
            let path = self.path("config.toml");
            !path.exists()
                || config::load_from(&path)
                    .unwrap()
                    .field_bool("sounds_enabled")
                    .unwrap()
        }

        fn words(&self) -> Vec<String> {
            DictionaryStore::new(Some(self.path("dictionary.json"))).words()
        }

        fn undo_exists(&self) -> bool {
            Path::new(&self.path("settings-undo.json")).exists()
        }
    }

    const PREVIEW: &str = "Preview (runs on your computer with gemma4:e4b):\n  \
        Sounds: On → Off\n  Dictionary: — → add “Kubernetes”\n  \
        WARNING: Sounds: On → Off — Check: turns Sounds off.\n";
    const PROMPT: &str = "Apply these changes? [y/N]: ";

    #[test]
    fn preview_then_cancel_changes_nothing() {
        let env = Env::new();
        for input in ["n\n", "\n", "NO\n"] {
            let result = env.run(&["turn off sounds and learn Kubernetes"], input);
            assert_eq!(result.code, 0, "{}", result.err);
            assert_eq!(result.out, format!("{PREVIEW}{PROMPT}Nothing changed.\n"));
            assert_eq!(result.err, "");
            assert!(env.sounds());
            assert!(env.words().is_empty());
            assert!(!env.undo_exists());
        }
    }

    #[test]
    fn confirm_applies_and_undo_reverts() {
        let env = Env::new();
        let result = env.run(&["turn off sounds and learn Kubernetes"], " Y \n");
        assert_eq!(result.code, 0, "{}", result.err);
        assert_eq!(
            result.out,
            format!("{PREVIEW}{PROMPT}Applied 2 changes.\nUndo with: lf tweak --undo\n")
        );
        assert!(!env.sounds());
        assert_eq!(env.words(), ["Kubernetes"]);
        assert!(env.undo_exists());

        let requests = env.ollama.requests.borrow().len();
        let undone = env.run(&["--undo"], "");
        assert_eq!(undone.code, 0, "{}", undone.err);
        assert_eq!(
            undone.out,
            "  Sounds: On\n  Removed word “Kubernetes”\nUndid the last change.\n"
        );
        assert!(env.sounds());
        assert!(env.words().is_empty());
        assert!(!env.undo_exists());
        // Undo is local file work: it never calls the model.
        assert_eq!(env.ollama.requests.borrow().len(), requests);

        let again = env.run(&["--undo"], "");
        assert_eq!(
            (again.code, again.out.as_str(), again.err.as_str()),
            (1, "", "Error: There's no change to undo.\n")
        );
    }

    #[test]
    fn yes_skips_the_prompt() {
        let env = Env::new();
        let result = env.run(&["turn off sounds", "--yes"], "");
        assert_eq!(result.code, 0, "{}", result.err);
        assert!(!result.out.contains("Apply these changes?"));
        // The unrequested word is refused, not applied.
        assert_eq!(
            result.out,
            "Preview (runs on your computer with gemma4:e4b):\n  Sounds: On → Off\n  \
             WARNING: Sounds: On → Off — Check: turns Sounds off.\n  skipped: Adding \
             “Kubernetes” wasn't in your request, so it was left out; ask for it by name\n\
             Applied 1 change.\nUndo with: lf tweak --undo\n"
        );
        assert!(!env.sounds());
        assert!(env.words().is_empty());
    }

    #[test]
    fn eof_at_the_prompt_aborts_without_changes() {
        let env = Env::new();
        for input in ["", "maybe\n"] {
            let result = env.run(&["turn off sounds and learn Kubernetes"], input);
            assert_eq!(result.code, 1);
            let retry = if input.is_empty() {
                String::new()
            } else {
                format!("Error: invalid input\n{PROMPT}")
            };
            assert_eq!(result.out, format!("{PREVIEW}{PROMPT}{retry}"));
            assert_eq!(result.err, "Aborted!\n");
            assert!(env.sounds());
            assert!(!env.undo_exists());
        }
        let retried = env.run(&["turn off sounds and learn Kubernetes"], "maybe\nyes\n");
        assert_eq!(retried.code, 0);
        assert!(retried.out.contains("Error: invalid input\n"));
        assert!(!env.sounds());
    }

    #[test]
    fn missing_request_and_bad_flags_are_usage_errors_without_the_model() {
        let env = Env::new();
        for (args, message) in [
            (
                &[][..],
                "say what to change, e.g. lf tweak \"turn off sounds\"",
            ),
            (
                &["   "],
                "say what to change, e.g. lf tweak \"turn off sounds\"",
            ),
            (
                &["--yes"],
                "say what to change, e.g. lf tweak \"turn off sounds\"",
            ),
            (
                &["--undo", "turn off sounds"],
                "use either a request or --undo, not both",
            ),
            (&["x", "--undo"], "use either a request or --undo, not both"),
            (&["--undo", "--yes"], "--yes only applies to a request"),
            (&["-y", "--undo"], "--yes only applies to a request"),
        ] {
            let result = env.run(args, "y\n");
            assert_eq!(
                (result.code, result.out.as_str(), result.err.as_str()),
                (2, "", format!("Error: {message}\n").as_str()),
                "{args:?}"
            );
        }
        // Parser-level errors keep clap's own wording.
        for args in [
            &["--bogus", "x"][..],
            &["-yx", "a"],
            &["--yes=1", "a"],
            &["a", "b"],
        ] {
            let result = env.run(args, "y\n");
            assert_eq!((result.code, result.out.as_str()), (2, ""), "{args:?}");
            assert!(!result.err.is_empty(), "{args:?}");
        }
        assert_eq!(*env.agents.borrow(), 0);
        assert!(env.ollama.requests.borrow().is_empty());
        assert!(!env.path("config.toml").exists());
    }

    #[test]
    fn help_and_repeated_flags_are_accepted() {
        let help = run_cli(&["tweak", "--help"]);
        assert_eq!(help.code, 0);
        assert!(help.out.contains("--undo") && help.out.contains("-y, --yes"));
        assert!(help.out.contains("Shows a preview first"));
        let env = Env::new();
        let result = env.run(&["--yes", "turn off sounds", "-y"], "");
        assert_eq!(result.code, 0, "{}", result.err);
        // After `--`, "--undo" is the request; the model's calls don't match it.
        let request = Env::new().run(&["--", "--undo"], "");
        assert_eq!(request.code, 1);
        assert!(request
            .err
            .starts_with("Error: Nothing to change: Sounds wasn't"));
    }

    #[test]
    fn agent_errors_are_clean_messages() {
        let mut env = Env::new();
        env.ollama.unreachable = true;
        let result = env.run(&["turn off sounds", "--yes"], "");
        assert_eq!(
            (result.code, result.out.as_str(), result.err.as_str()),
            (
                1,
                "",
                "Error: Couldn't reach Ollama at http://localhost:11434. \
                 Start Ollama, then try again.\n"
            )
        );
        assert!(env.sounds());

        env.ollama.unreachable = false;
        for (calls, message) in [
            (vec![], "The local model didn't suggest any setting change. Try naming the setting, e.g. “turn off sounds”."),
            (
                vec![call("set_setting", json!({"key": "server_port", "value": "1"})); 11],
                "The local model asked for more than 10 changes at once; ask for fewer.",
            ),
            (
                vec![call("set_setting", json!({"key": "server_port", "value": "1"}))],
                "Nothing to change: 'server_port' is not a setting LocalFlow can change here",
            ),
        ] {
            env.ollama.calls = calls;
            let result = env.run(&["turn off sounds"], "y\n");
            assert_eq!(
                (result.code, result.out.as_str(), result.err.as_str()),
                (1, "", format!("Error: {message}\n").as_str())
            );
        }
        // Escaped by the rejection, again by propose and again by the CLI, as
        // in Python.
        env.ollama.calls = vec![call(
            "set_setting",
            json!({"key": "sounds\u{2192}x", "value": "off"}),
        )];
        let escaped = env.run(&["turn off sounds"], "");
        assert_eq!(
            escaped.err,
            "Error: Nothing to change: 'sounds\\u005cu005cu2192x' is not a setting \
             LocalFlow can change here\n"
        );
        assert!(!env.undo_exists());
    }
}
