//! The Ask backend: conversation state plus fresh retrieval and answering.
//!
//! Port of the pure data and method surface of `localflow/ask_model.py` that
//! `ask_mac.py` / `hub_mac.py` use: `EXAMPLES`, `PRIVACY`, `TIME_FILTERS`,
//! `SOURCES`, `Turn`, `citation_target`, `citation_label` and the `AskModel`
//! methods (`turns`, `clear`, `ask`). Retrieval itself (`recall`) and the LLM
//! call are *not* ported: a real backend implements [`AskBackend`], and
//! [`FakeAskBackend`] is the in-memory stand-in for tests.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::pystr::PyStr;

pub const EXAMPLES: [&str; 3] = [
    "What did we decide in our last meeting?",
    "What are my next steps for the project?",
    "What have I dictated about the launch?",
];
pub const PRIVACY: &str = "Answers use the model selected on the Models page. \
If you select a cloud provider, it will receive the matched snippets \
and conversation context.";
/// `TIME_FILTERS` in insertion order: popup title -> `since` (`None` = any).
pub const TIME_FILTERS: [(&str, Option<&str>); 3] = [
    ("Any time", None),
    ("Past week", Some("7d")),
    ("Past month", Some("30d")),
];
/// `SOURCES` in insertion order: key -> display name.
pub const SOURCES: [(&str, &str); 3] = [
    ("dictation", "Dictation"),
    ("meeting", "Meeting"),
    ("notes", "Note"),
];
/// `recall.NO_ANSWER`.
pub const NO_ANSWER: &str = "I couldn't find that in your history.";
/// `ask_mac.INTRO` / `ask_mac.THINKING` (the page's fixed strings).
pub const INTRO: &str = "Find decisions, ideas, and next steps across your local sources.";
pub const THINKING: &str = "Thinking...";

/// Error texts `AskModel.ask` stores on a failed turn.
pub const ERROR_EMPTY_QUESTION: &str = "Type a question to get started.";
pub const ERROR_NO_SOURCES: &str = "Choose at least one source to search.";
pub const ERROR_UNREACHABLE: &str =
    "Couldn't reach the model or local sources. Check the Models page and try again.";
pub const ERROR_GENERIC: &str = "Couldn't answer that question. Please try again.";

/// `since` for a time-filter title, `None` for an unknown title.
pub fn time_filter_since(title: &str) -> Option<Option<&'static str>> {
    TIME_FILTERS
        .iter()
        .find(|(name, _)| *name == title)
        .map(|(_, since)| *since)
}

/// Display name of a source key.
pub fn source_label(key: &str) -> Option<&'static str> {
    SOURCES.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// One citation dict (`recall._citation`).
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct Citation {
    pub n: i64,
    pub source: String,
    pub title: String,
    pub app: String,
    /// ISO date-time to the minute, or "".
    pub when: String,
    pub snippet: String,
    /// History id (dictation), file path (notes) or `path#timestamp` (meeting).
    #[serde(rename = "ref")]
    pub reference: String,
}

/// One question/answer exchange.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Turn {
    pub question: String,
    pub answer: String,
    pub citations: Vec<Citation>,
    pub error: String,
}

impl Turn {
    pub fn new(question: impl Into<String>) -> Self {
        Turn {
            question: question.into(),
            ..Turn::default()
        }
    }
}

/// Where a citation leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CitationTarget {
    /// A history entry id.
    History(i64),
    /// A source file, without a transcript timestamp.
    File(String),
}

/// A citation that cannot be resolved (Python raises `KeyError`/`ValueError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CitationError(pub String);

impl std::fmt::Display for CitationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CitationError {}

/// A history id or a source file, without a transcript timestamp.
pub fn citation_target(citation: &Citation) -> Result<CitationTarget, CitationError> {
    match citation.source.as_str() {
        "dictation" => citation
            .reference
            .py_strip()
            .parse::<i64>()
            .map(CitationTarget::History)
            .map_err(|_| {
                CitationError(format!(
                    "invalid history reference: {:?}",
                    citation.reference
                ))
            }),
        "meeting" => Ok(CitationTarget::File(
            citation
                .reference
                .rsplit_once('#')
                .map_or(citation.reference.as_str(), |(path, _)| path)
                .to_string(),
        )),
        _ => Ok(CitationTarget::File(citation.reference.clone())),
    }
}

/// Resolve a document citation inside the configured vault, never a launch target.
pub fn citation_document(path: &str, vault: &str) -> Option<PathBuf> {
    if vault.py_strip().is_empty() {
        return None;
    }
    let root = lf_core::home::expand_user(Path::new(vault))
        .ok()?
        .canonicalize()
        .ok()?;
    let path = lf_core::home::expand_user(Path::new(path))
        .ok()?
        .canonicalize()
        .ok()?;
    if !root.is_dir() || !path.starts_with(&root) || !path.is_file() {
        return None;
    }
    if path.ancestors().any(|part| {
        part.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("app") || ext.eq_ignore_ascii_case("bundle")
        })
    }) {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "md" | "markdown" | "txt" | "pdf" | "rtf") {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.metadata().ok()?.permissions().mode() & 0o111 != 0 {
            return None;
        }
    }
    Some(path)
}

/// `[n] Source · YYYY-MM-DD · title`; the date is the first 10 characters of
/// `when` (Unicode scalars, like Python slicing), "Unknown date" when empty.
pub fn citation_label(citation: &Citation) -> Result<String, CitationError> {
    let source = source_label(&citation.source)
        .ok_or_else(|| CitationError(format!("unknown source: {:?}", citation.source)))?;
    let date: String = citation.when.chars().take(10).collect();
    let date = if date.is_empty() {
        "Unknown date".to_string()
    } else {
        date
    };
    Ok(format!(
        "[{}] {source} · {date} · {}",
        citation.n, citation.title
    ))
}

/// What the Ask page (and the Hub worker thread) needs from `AskModel`.
pub trait AskBackend: Send {
    /// The conversation so far (`model.turns`).
    fn turns(&self) -> Vec<Turn>;
    /// Replace the conversation (`model.turns = ...` after a worker finished).
    fn set_turns(&mut self, turns: Vec<Turn>);
    /// Forget the conversation (`model.clear()`).
    fn clear(&mut self);
    /// Answer `question`, append the turn to the conversation and return it.
    /// Blocking; called on a worker. Failures are stored in `Turn::error`,
    /// never raised. `sources` is `None` for "all sources".
    fn ask(
        &mut self,
        question: &str,
        time_filter: &str,
        sources: Option<&BTreeSet<String>>,
        now: Option<f64>,
    ) -> Turn;
}

/// A scripted reply of the fake backend.
#[derive(Clone, Debug, PartialEq)]
pub enum FakeReply {
    /// The model's answer text with the citations the retrieval produced.
    Answer {
        text: String,
        citations: Vec<Citation>,
    },
    /// Retrieval found nothing (`recall.NO_ANSWER`).
    NothingFound,
    /// A `ProviderError` / `OSError` from the model or the sources.
    Unreachable,
    /// Any other failure.
    Failure,
}

/// One recorded call to [`FakeAskBackend::ask`] (after question trimming).
#[derive(Clone, Debug, PartialEq)]
pub struct AskCall {
    pub question: String,
    pub time_filter: String,
    pub since: Option<String>,
    pub sources: Option<Vec<String>>,
    pub now: Option<f64>,
}

/// In-memory [`AskBackend`]: same validation and failure mapping as
/// `AskModel.ask`, replies come from a script instead of recall + an LLM.
/// With an empty script every question finds nothing (an empty corpus).
#[derive(Clone, Debug, Default)]
pub struct FakeAskBackend {
    pub turns: Vec<Turn>,
    pub script: VecDeque<FakeReply>,
    pub calls: Vec<AskCall>,
}

impl FakeAskBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_script(replies: impl IntoIterator<Item = FakeReply>) -> Self {
        FakeAskBackend {
            script: replies.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl AskBackend for FakeAskBackend {
    fn turns(&self) -> Vec<Turn> {
        self.turns.clone()
    }

    fn set_turns(&mut self, turns: Vec<Turn>) {
        self.turns = turns;
    }

    fn clear(&mut self) {
        self.turns.clear();
    }

    fn ask(
        &mut self,
        question: &str,
        time_filter: &str,
        sources: Option<&BTreeSet<String>>,
        now: Option<f64>,
    ) -> Turn {
        let question = question.py_strip();
        let mut turn = Turn::new(question);
        if question.is_empty() {
            // Python returns before appending the turn.
            turn.error = ERROR_EMPTY_QUESTION.into();
            return turn;
        }
        let since = time_filter_since(time_filter);
        self.calls.push(AskCall {
            question: question.to_string(),
            time_filter: time_filter.to_string(),
            since: since.flatten().map(str::to_string),
            sources: sources.map(|s| s.iter().cloned().collect()),
            now,
        });
        if since.is_none() {
            turn.error = ERROR_GENERIC.into();
        } else if sources.is_some_and(BTreeSet::is_empty) {
            turn.error = ERROR_NO_SOURCES.into();
        } else {
            match self.script.pop_front().unwrap_or(FakeReply::NothingFound) {
                FakeReply::NothingFound => turn.answer = NO_ANSWER.into(),
                FakeReply::Unreachable => turn.error = ERROR_UNREACHABLE.into(),
                FakeReply::Failure => turn.error = ERROR_GENERIC.into(),
                FakeReply::Answer { text, citations } => {
                    let answer = text.py_strip();
                    let no_answer = NO_ANSWER.to_lowercase();
                    let no_answer = no_answer.trim_end_matches('.');
                    if answer.is_empty() || answer.to_lowercase().contains(no_answer) {
                        turn.answer = NO_ANSWER.into();
                    } else {
                        turn.answer = answer.to_string();
                        turn.citations = citations;
                    }
                }
            }
        }
        self.turns.push(turn.clone());
        turn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(source: &str, reference: &str, when: &str) -> Citation {
        Citation {
            n: 2,
            source: source.into(),
            title: "Launch planning".into(),
            when: when.into(),
            reference: reference.into(),
            ..Citation::default()
        }
    }

    #[test]
    fn constants_keep_python_order() {
        assert_eq!(
            TIME_FILTERS.map(|(t, _)| t),
            ["Any time", "Past week", "Past month"]
        );
        assert_eq!(SOURCES.map(|(k, _)| k), ["dictation", "meeting", "notes"]);
        assert_eq!(time_filter_since("Past week"), Some(Some("7d")));
        assert_eq!(time_filter_since("Any time"), Some(None));
        assert_eq!(time_filter_since("Yesterday"), None);
    }

    #[test]
    fn citation_labels_and_targets() {
        let c = cite("meeting", "notes/a.md#12:30", "2026-04-11T10:15");
        assert_eq!(
            citation_label(&c).unwrap(),
            "[2] Meeting · 2026-04-11 · Launch planning"
        );
        assert_eq!(
            citation_target(&c).unwrap(),
            CitationTarget::File("notes/a.md".into())
        );
        let d = cite("dictation", "42", "");
        assert_eq!(
            citation_label(&d).unwrap(),
            "[2] Dictation · Unknown date · Launch planning"
        );
        assert_eq!(citation_target(&d).unwrap(), CitationTarget::History(42));
        let n = cite("notes", "n#x", "2026-04-11");
        assert_eq!(
            citation_target(&n).unwrap(),
            CitationTarget::File("n#x".into())
        );
        assert!(citation_target(&cite("dictation", "abc", "")).is_err());
        assert!(citation_label(&cite("web", "", "")).is_err());
    }

    #[test]
    fn citation_documents_reject_launch_targets_and_escape_paths() {
        let vault = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let document = vault.path().join("notes.md");
        std::fs::write(&document, "notes").unwrap();
        let root = vault.path().to_str().unwrap();
        assert_eq!(
            citation_document(document.to_str().unwrap(), root),
            Some(document.canonicalize().unwrap())
        );
        for name in ["missing.md", "run.command", "script.sh", "file.webloc", ""] {
            if name != "missing.md" && !name.is_empty() {
                std::fs::write(vault.path().join(name), "not a document").unwrap();
            }
            assert!(citation_document(vault.path().join(name).to_str().unwrap(), root).is_none());
        }
        let app = vault.path().join("Malicious.app");
        std::fs::create_dir(&app).unwrap();
        std::fs::write(app.join("readme.md"), "notes").unwrap();
        assert!(matches!(
            citation_target(&cite("notes", app.to_str().unwrap(), "")).unwrap(),
            CitationTarget::File(_)
        ));
        assert!(citation_document(app.to_str().unwrap(), root).is_none());
        assert!(citation_document(app.join("readme.md").to_str().unwrap(), root).is_none());
        let other = outside.path().join("secret.md");
        std::fs::write(&other, "secret").unwrap();
        assert!(citation_document(other.to_str().unwrap(), root).is_none());
        assert!(citation_document(document.to_str().unwrap(), "").is_none());
        assert!(citation_document("https://example.com/file.md", root).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let link = vault.path().join("escape.md");
            symlink(&other, &link).unwrap();
            assert!(citation_document(link.to_str().unwrap(), root).is_none());
            std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(citation_document(document.to_str().unwrap(), root).is_none());
        }
    }

    #[test]
    fn date_slice_counts_characters_not_bytes() {
        let c = cite("notes", "n", "日本語日本語日本語日本語日本語");
        assert_eq!(
            citation_label(&c).unwrap(),
            "[2] Note · 日本語日本語日本語日 · Launch planning"
        );
    }

    #[test]
    fn fake_validates_like_ask_model() {
        let mut fake = FakeAskBackend::new();
        let empty = fake.ask("   ", "Any time", None, None);
        assert_eq!(empty.error, ERROR_EMPTY_QUESTION);
        assert!(fake.turns.is_empty() && fake.calls.is_empty());

        let none: BTreeSet<String> = BTreeSet::new();
        assert_eq!(
            fake.ask("q", "Any time", Some(&none), None).error,
            ERROR_NO_SOURCES
        );
        assert_eq!(fake.ask("q", "Someday", None, None).error, ERROR_GENERIC);
        assert_eq!(fake.turns.len(), 2);
        // An empty corpus finds nothing.
        let found = fake.ask("  what?  ", "Past month", None, Some(5.0));
        assert_eq!(
            (found.question.as_str(), found.answer.as_str()),
            ("what?", NO_ANSWER)
        );
        assert_eq!(fake.calls[2].since.as_deref(), Some("30d"));
        assert_eq!(fake.calls[2].now, Some(5.0));
        fake.clear();
        assert!(fake.turns().is_empty());
    }

    #[test]
    fn fake_replays_scripted_answers_and_failures() {
        let c = cite("meeting", "a#1", "2026-01-01");
        let mut fake = FakeAskBackend::with_script([
            FakeReply::Answer {
                text: " Ship it [1]. ".into(),
                citations: vec![c.clone()],
            },
            FakeReply::Answer {
                text: "I couldn't find that in your history".into(),
                citations: vec![c],
            },
            FakeReply::Unreachable,
            FakeReply::Failure,
        ]);
        let sources: BTreeSet<String> = ["meeting".to_string()].into();
        let a = fake.ask("q1", "Any time", Some(&sources), None);
        assert_eq!((a.answer.as_str(), a.citations.len()), ("Ship it [1].", 1));
        let b = fake.ask("q2", "Any time", None, None);
        assert_eq!((b.answer.as_str(), b.citations.len()), (NO_ANSWER, 0));
        assert_eq!(
            fake.ask("q3", "Any time", None, None).error,
            ERROR_UNREACHABLE
        );
        assert_eq!(fake.ask("q4", "Any time", None, None).error, ERROR_GENERIC);
        assert_eq!(fake.turns().len(), 4);
        let copy = fake.turns();
        fake.set_turns(vec![]);
        assert!(fake.turns.is_empty());
        fake.set_turns(copy);
        assert_eq!(fake.turns.len(), 4);
        assert_eq!(
            fake.calls[0].sources.as_deref(),
            Some(&["meeting".to_string()][..])
        );
    }
}
