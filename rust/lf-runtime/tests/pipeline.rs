//! Mirrors tests/test_process_clip.py: stage order, hot phrases, gates.

use std::sync::Mutex;

use lf_core::config::Config;
use lf_providers::base::{Health, ModelInfo, ProviderError};
use lf_providers::LlmProvider;
use lf_runtime::controller::{Job, JobKind};
use lf_runtime::engine::SpeechEngine;
use lf_runtime::pipeline::{process_clip, process_job, Models, Vocabulary};
use serde_json::json;

struct Speech {
    text: String,
    prompts: Mutex<Vec<Option<String>>>,
}

impl SpeechEngine for Speech {
    fn load(&self) -> Result<(), String> {
        Ok(())
    }
    fn transcribe(&self, _pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.prompts.lock().unwrap().push(prompt.map(str::to_string));
        Ok(self.text.clone())
    }
    fn unload(&self) {}
}

fn speech(text: &str) -> Speech {
    Speech {
        text: text.into(),
        prompts: Mutex::new(Vec::new()),
    }
}

/// The cleanup LLM: applies `f` to the text it receives and records it.
struct Llm {
    f: fn(&str) -> String,
    seen: Mutex<Vec<String>>,
}

impl LlmProvider for Llm {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(&self, _system: &str, user: &str, _max: Option<i64>) -> Result<String, ProviderError> {
        let text = user.strip_prefix("Text:\n").unwrap_or(user);
        self.seen.lock().unwrap().push(text.to_string());
        Ok((self.f)(text))
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}

fn llm(f: fn(&str) -> String) -> Llm {
    Llm {
        f,
        seen: Mutex::new(Vec::new()),
    }
}

fn config(cleanup: bool, hot_phrases: bool) -> Config {
    let mut c = Config::default();
    c.set_field("cleanup_enabled", json!(cleanup));
    c.set_field("cleanup_level", json!("light"));
    c.set_field("styles_enabled", json!(false));
    c.set_field("spoken_symbols", json!(true));
    c.set_field("spoken_punctuation", json!(true));
    c.set_field("hot_phrases", json!(hot_phrases));
    c
}

fn vocab(phrases: &[(&str, &str)]) -> Vocabulary {
    Vocabulary {
        words: Vec::new(),
        replacements: Vec::new(),
        hot_phrases: phrases.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        commands: vec!["skill_part".into(), "candidate-solutioning".into()],
    }
}

fn loud() -> Vec<f32> {
    vec![0.1; 16000]
}

fn run(text: &str, config: &Config, llm: Option<&Llm>, vocab: &Vocabulary) -> String {
    let s = speech(text);
    let models = Models {
        speech: &s,
        llm: llm.map(|l| l as &dyn LlmProvider),
    };
    process_clip(&loud(), config, &models, vocab, None).unwrap().1
}

#[test]
fn normalizes_known_command_after_cleanup() {
    for cleanup in [true, false] {
        let l = llm(|t| t.replace("Run", "Please run"));
        let out = run("Run slash skill part with these arguments.", &config(cleanup, true), Some(&l), &vocab(&[]));
        let expected = if cleanup { "Please run" } else { "Run" };
        assert_eq!(out, format!("{expected} /skill_part with these arguments."));
        // Cleanup saw the text before symbols ran.
        let seen = l.seen.lock().unwrap();
        assert_eq!(seen.len(), usize::from(cleanup));
        if cleanup {
            assert!(seen[0].contains("slash skill part"), "{seen:?}");
        }
    }
}

#[test]
fn short_command_skips_cleanup() {
    let l = llm(|_| panic!("cleanup must not run"));
    assert_eq!(
        run("Slash candidate solutioning.", &config(true, true), Some(&l), &vocab(&[])),
        "/candidate-solutioning"
    );
}

#[test]
fn whole_match_skips_cleanup_and_symbols() {
    let l = llm(|_| panic!("cleanup must not run"));
    let v = vocab(&[("review checklist", "check slash dash back\nsecond line")]);
    assert_eq!(
        run("Review checklist.", &config(true, true), Some(&l), &v),
        "check slash dash back\nsecond line"
    );
}

#[test]
fn inline_expansion_after_cleanup() {
    let l = llm(|t| t.to_uppercase());
    let v = vocab(&[("review checklist", "CHECK IT")]);
    assert_eq!(
        run("please review checklist then commit now", &config(true, true), Some(&l), &v),
        "PLEASE CHECK IT THEN COMMIT NOW"
    );
}

#[test]
fn trigger_with_spoken_symbol_word_expands() {
    let v = vocab(&[("review dash checklist", "CHECK IT")]);
    assert_eq!(
        run("please review dash checklist now", &config(false, true), None, &v),
        "please CHECK IT now"
    );
}

#[test]
fn expansion_is_not_symbol_converted() {
    let v = vocab(&[("review checklist", "use slash here")]);
    assert_eq!(
        run("a dash b review checklist", &config(false, true), None, &v),
        "a-b use slash here"
    );
}

#[test]
fn hot_phrases_disabled_in_config() {
    let v = vocab(&[("review checklist", "CHECK IT")]);
    assert_eq!(run("review checklist", &config(false, false), None, &v), "review checklist");
}

#[test]
fn dictionary_replacement_runs_before_cleanup_and_prompts_stt() {
    let l = llm(|t| t.to_string());
    let mut v = vocab(&[]);
    v.words = vec!["Kubernetes".into()];
    v.replacements = vec![("cube nets".into(), "Kubernetes".into())];
    let s = speech("we deploy to cube nets every day");
    let models = Models {
        speech: &s,
        llm: Some(&l),
    };
    let (_, out) = process_clip(&loud(), &config(true, true), &models, &v, None).unwrap();
    assert_eq!(out, "we deploy to Kubernetes every day");
    assert!(l.seen.lock().unwrap()[0].contains("Kubernetes"));
    assert!(s.prompts.lock().unwrap()[0].as_deref().unwrap().contains("Kubernetes"));
}

#[test]
fn style_runs_after_symbols() {
    let mut c = config(false, true);
    c.set_field("styles_enabled", json!(true));
    c.set_field("style_other", json!("very_casual"));
    let out = run("Hello World dash test.", &c, None, &vocab(&[]));
    assert_eq!(out, lf_core::formatting::apply_style("Hello World-test.", "very_casual", &[] as &[String]));
}

#[test]
fn silence_skips_transcriber() {
    let s = speech("should not be used");
    let models = Models { speech: &s, llm: None };
    let (raw, text) = process_clip(&vec![0.0; 16000], &config(true, true), &models, &vocab(&[]), None).unwrap();
    assert_eq!((raw.as_str(), text.as_str()), ("", ""));
    assert!(s.prompts.lock().unwrap().is_empty());
}

#[test]
fn process_job_records_history_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let history = lf_core::history::HistoryStore::open(
        Some(dir.path().join("history.db")),
        Some(dir.path().join("clips")),
        true,
    )
    .unwrap();
    let s = speech("hello there");
    let models = Models { speech: &s, llm: None };
    let job = Job {
        kind: JobKind::Dictate,
        audio: Some(loud()),
        app_info: None,
        selection: String::new(),
        preset: None,
        clipboard_before: None,
    };
    let (text, error) = process_job(&job, &config(false, true), &models, &vocab(&[]), Some(&history));
    assert_eq!(text.as_deref(), Some("hello there"));
    assert!(!error);
    assert_eq!(history.latest_text("").unwrap(), "hello there");

    // Silence: no text, status "silent", and the clip is deleted.
    let quiet = Job {
        audio: Some(vec![0.0; 16000]),
        ..job.clone()
    };
    let (text, error) = process_job(&quiet, &config(false, true), &models, &vocab(&[]), Some(&history));
    assert_eq!((text, error), (None, false));
}

#[test]
fn transform_job_uses_the_llm() {
    let l = llm(|t| format!("\"Text: {}\"", t.lines().last().unwrap_or_default()));
    let s = speech("");
    let models = Models { speech: &s, llm: Some(&l) };
    let job = Job {
        kind: JobKind::Transform,
        audio: None,
        app_info: None,
        selection: "rough draft".into(),
        preset: Some("polish".into()),
        clipboard_before: None,
    };
    let (text, error) = process_job(&job, &config(true, true), &models, &vocab(&[]), None);
    assert_eq!(text.as_deref(), Some("rough draft"));
    assert!(!error);
}
