//! `--sandbox`: a persistent, isolated data directory with example data.
//!
//! The directory is seeded once (marker file) and kept between launches so
//! edits can be checked after a relaunch. `--sandbox-reset` re-seeds it.
//! Services are in-process fakes; no socket is opened and the OS keyring is
//! never used.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use lf_core::config::{self, Config};
use lf_core::dictionary::DictionaryStore;
use lf_core::history::NewEntry;
use lf_core::hotphrases::HotPhraseStore;
use lf_ui::backends::meeting::FakeMeetingClient;
use lf_ui::backends::prompts::{FakePromptQueue, PromptQueue, WorkPrompt};
use serde_json::{json, Map, Value};

use crate::data::{open_history, DataRoot, SANDBOX_MARKER};

const MARKER_TEXT: &str = "This directory is a LocalFlow desktop sandbox.\n\
It holds example data only. Nothing here is your real LocalFlow data.\n\
Delete it at any time, or run `lf-desktop --sandbox --sandbox-reset`.\n";

/// Example dictations: `(minutes ago, app, category, raw, final, status)`.
const HISTORY: &[(i64, &str, &str, &str, &str, &str)] = &[
    (
        12,
        "Mail",
        "email",
        "hi sam thanks for the notes from yesterday i will send the revised budget by friday",
        "Hi Sam, thanks for the notes from yesterday. I'll send the revised budget by Friday.",
        "ok",
    ),
    (
        47,
        "Terminal",
        "work",
        "git commit dash m fix the off by one in the pager",
        "git commit -m \"Fix the off-by-one in the pager\"",
        "ok",
    ),
    (
        95,
        "Editor",
        "work",
        "todo move the sandbox seed into its own module and add a reset flag",
        "TODO: move the sandbox seed into its own module and add a reset flag.",
        "ok",
    ),
    (
        180,
        "Chat",
        "personal",
        "running ten minutes late see you at the cafe",
        "Running ten minutes late, see you at the café!",
        "ok",
    ),
    (240, "Editor", "work", "", "", "silent"),
    (
        1500,
        "Browser",
        "other",
        "search for local speech recognition benchmarks twenty twenty six",
        "Search for local speech recognition benchmarks 2026",
        "ok",
    ),
    (
        1560,
        "Notes",
        "personal",
        "ideas for the trip hiking in the dolomites book the hut early",
        "Ideas for the trip: hiking in the Dolomites, book the hut early.",
        "ok",
    ),
    (
        1620,
        "Mail",
        "email",
        "the quarterly review moved to thursday at two",
        "The quarterly review moved to Thursday at 2 PM.",
        "ok",
    ),
    (
        2950,
        "Editor",
        "work",
        "kubernetes rollout failed on the canary check the readiness probe",
        "Kubernetes rollout failed on the canary; check the readiness probe.",
        "ok",
    ),
    (
        3000,
        "Notes",
        "other",
        "会議は木曜日の午後二時に移動しました",
        "会議は木曜日の午後二時に移動しました。",
        "ok",
    ),
    (
        3010,
        "Notes",
        "other",
        "تم نقل الاجتماع إلى يوم الخميس",
        "تم نقل الاجتماع إلى يوم الخميس.",
        "ok",
    ),
    (4400, "Editor", "work", "", "", "failed"),
    (
        5800,
        "Chat",
        "personal",
        "happy birthday hope you have a great day",
        "Happy birthday! Hope you have a great day.",
        "ok",
    ),
    (
        7300,
        "Mail",
        "email",
        "please find the signed contract attached",
        "Please find the signed contract attached.",
        "ok",
    ),
];

const WORDS: &[&str] = &["LocalFlow", "Kubernetes", "Ollama", "Dolomites", "GPUI"];
const REPLACEMENTS: &[(&str, &str)] = &[
    ("teh", "the"),
    ("my email", "sam@example.com"),
    ("k8s", "Kubernetes"),
];
const SNIPPETS: &[(&str, &str, bool)] = &[
    ("my address", "221B Example Street, Springfield", true),
    (
        "standup template",
        "Yesterday:\n- \nToday:\n- \nBlockers:\n- ",
        true,
    ),
    ("sign off", "Best regards,\nSam", false),
];

/// Seed `dir` unless already seeded (or `reset`). Returns the data root.
pub fn prepare(dir: &Path, reset: bool) -> Result<DataRoot> {
    let marker = dir.join(SANDBOX_MARKER);
    if reset && dir.exists() {
        // Only ever delete a directory we seeded ourselves.
        anyhow::ensure!(
            marker.exists(),
            "{} is not a LocalFlow sandbox (no {SANDBOX_MARKER}); refusing to reset it",
            dir.display()
        );
        fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    let root = DataRoot::sandbox(dir);
    if marker.exists() {
        return Ok(root);
    }
    if dir.exists() && fs::read_dir(dir)?.next().is_some() {
        anyhow::bail!(
            "{} exists and is not a LocalFlow sandbox; choose an empty directory",
            dir.display()
        );
    }
    fs::create_dir_all(&root.localflow_dir)?;
    seed(&root)?;
    fs::write(&marker, MARKER_TEXT)?;
    Ok(root)
}

fn seed(root: &DataRoot) -> Result<()> {
    let base = root.sandbox_dir.clone().expect("sandbox root");
    let vault = base.join("vault");
    let notes = vault.join("notes").join("meetings");
    fs::create_dir_all(&notes)?;
    fs::write(
        notes.join("2026-11-18 1000 Roadmap sync.md"),
        "# Roadmap sync (example)\n\n## Summary\n- Native desktop app ships beside the existing Hub.\n\n## Action items\n- [ ] Review the cutover checklist\n",
    )?;
    fs::write(
        notes.join("2026-11-12 1530 Design review.md"),
        "# Design review (example)\n\n## Summary\n- Warm stone palette, forest accent.\n",
    )?;
    fs::create_dir_all(base.join("templates"))?;

    let mut cfg = Config::defaults_for(cfg!(target_os = "macos"));
    let set = |cfg: &mut Config, field: &str, value: Value| cfg.set_field(field, value);
    set(&mut cfg, "meetings_enabled", json!(true));
    set(&mut cfg, "skills_dirs", json!([]));
    set(&mut cfg, "vault_path", json!(vault.to_string_lossy()));
    set(
        &mut cfg,
        "prompts_dir",
        json!(base.join("prompts").to_string_lossy()),
    );
    set(
        &mut cfg,
        "meeting_templates_dir",
        json!(base.join("templates").to_string_lossy()),
    );
    // Port 9 (discard) is never served: the sandbox makes no HTTP calls anyway.
    set(&mut cfg, "server_port", json!(9));
    config::save_config(&cfg, Some(&root.config_path))?;

    let history = open_history(root)?;
    let now = chrono::Utc::now().timestamp() as f64;
    for (minutes, app, category, raw, text, status) in HISTORY {
        let mut entry = NewEntry::new("dictate");
        entry.app_name = Some((*app).into());
        entry.category = Some((*category).into());
        entry.raw_text = (*raw).into();
        entry.final_text = (*text).into();
        entry.status = (*status).into();
        entry.duration_s = 4.0 + (text.len() as f64 / 12.0);
        entry.ts = Some(now - (*minutes as f64) * 60.0);
        history.add(&entry)?;
    }

    let dictionary = DictionaryStore::new(Some(root.dictionary_path.clone()));
    for word in WORDS {
        dictionary.add_word(word)?;
    }
    for (from, to) in REPLACEMENTS {
        dictionary.add_replacement(from, to)?;
    }
    let phrases = HotPhraseStore::new(root.hot_phrases_path.clone());
    for (trigger, text, enabled) in SNIPPETS {
        phrases.add(trigger, text, *enabled)?;
    }
    Ok(())
}

/// The sandbox prompt queue: three example prompts, one failed.
pub fn seed_prompt_queue() -> FakePromptQueue {
    let queue = FakePromptQueue::new();
    let prompts = [
        WorkPrompt::new("Add a reset flag to the sandbox", "Example prompt."),
        WorkPrompt::new("Write the cutover checklist", "Example prompt."),
        WorkPrompt::new("Update deploy docs for Redis", "Example prompt."),
    ];
    let paths: Vec<String> = (1..=3).map(|i| format!("example/prompt-{i}.md")).collect();
    if let Ok(entries) = queue.add(&prompts, &paths, "Roadmap sync (example)") {
        if let Some(id) = entries
            .get(2)
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
        {
            let mut fields = Map::new();
            fields.insert("error".into(), json!("Orca is not running"));
            let _ = queue.set_status(id, "failed", None, &fields);
        }
    }
    queue
}

/// The sandbox meeting client: saved notes point into the sandbox vault.
pub fn meeting_client(root: &DataRoot) -> FakeMeetingClient {
    let client = FakeMeetingClient::new();
    if let Some(base) = &root.sandbox_dir {
        let notes = base
            .join("vault")
            .join("notes")
            .join("meetings")
            .join("2026-11-18 1000 Roadmap sync.md");
        client.set_saved_notes_path(&notes.to_string_lossy());
    }
    client.set_chat_answer(Some(
        "Sandbox answer: the example meeting agreed to ship the native app beside the existing Hub.",
    ));
    client
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_is_isolated_persistent_and_resettable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sb");
        let root = prepare(&base, false).unwrap();
        assert!(base.join(SANDBOX_MARKER).exists());
        let history = open_history(&root).unwrap();
        assert_eq!(history.recent(100, None, 0).unwrap().len(), HISTORY.len());
        // Edits survive a relaunch.
        let dict = DictionaryStore::new(Some(root.dictionary_path.clone()));
        dict.add_word("Persisted").unwrap();
        let again = prepare(&base, false).unwrap();
        let dict = DictionaryStore::new(Some(again.dictionary_path.clone()));
        assert!(dict.words().contains(&"Persisted".to_string()));
        // Reset re-seeds.
        let reset = prepare(&base, true).unwrap();
        let dict = DictionaryStore::new(Some(reset.dictionary_path.clone()));
        assert!(!dict.words().contains(&"Persisted".to_string()));
    }

    #[test]
    fn refuses_foreign_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("precious.txt"), "x").unwrap();
        assert!(prepare(dir.path(), false).is_err());
        assert!(prepare(dir.path(), true).is_err());
        assert!(dir.path().join("precious.txt").exists());
    }

    #[test]
    fn seed_queue_has_pending_and_failed() {
        let queue = seed_prompt_queue();
        let all = queue.list(None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(queue.list(Some("failed")).unwrap().len(), 1);
    }
}
