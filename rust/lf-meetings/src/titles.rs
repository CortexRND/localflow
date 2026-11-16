use crate::{neutralize, trim, words};
use chrono::{Datelike, NaiveDateTime, Timelike};
use lf_core::symbols::pycompat as py;
use lf_providers::{ollama::OllamaLlm, openai::OpenAiCompatLlm, LlmProvider};
use regex::Regex;
use std::{
    sync::{mpsc, Arc, LazyLock},
    time::Duration,
};
use unicode_general_category::{get_general_category as cat, GeneralCategory as G};
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_TITLE: &str = "Meeting";
pub const TITLE_TIMEOUT_SECONDS: f64 = 20.0;
pub const MAX_TITLE_TRANSCRIPT_CHARS: usize = 6000;
pub const MAX_TITLE_NOTES_CHARS: usize = 2000;
pub const MAX_TITLE_TOKENS: i64 = 24;
pub const MAX_TITLE_WORDS: usize = 6;
pub const MAX_GENERATED_TITLE_CHARS: usize = 60;
pub const MAX_NAME_BYTES: usize = 200;
pub const MAX_NAME_ATTEMPTS: usize = 100;
pub fn is_default_title(title: Option<&str>) -> bool {
    title.is_none_or(|s| {
        let s = words(s).collect::<Vec<_>>().join(" ");
        s.is_empty() || s.eq_ignore_ascii_case(DEFAULT_TITLE)
    })
}
pub fn fallback_title(started_at: NaiveDateTime) -> String {
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "Meeting {} {}, {}:{:02} {}",
        months[started_at.month0() as usize],
        started_at.day(),
        if started_at.hour().is_multiple_of(12) {
            12
        } else {
            started_at.hour() % 12
        },
        started_at.minute(),
        if started_at.hour() < 12 { "AM" } else { "PM" }
    )
}
static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:meeting[\s\x1c-\x1f]+)?title[\s\x1c-\x1f]*[:\-–—][\s\x1c-\x1f]*").unwrap()
});
pub fn clean_generated_title(text: &str) -> Option<String> {
    let line = crate::lines(text)
        .map(trim)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let line = line.trim_matches(|c| " #*>`\"'“”‘’".contains(c));
    let line = PREFIX.replace(line, "");
    let kept: String = line
        .nfkc()
        .map(|c| {
            if py::is_alnum(c)
                || matches!(cat(c), G::NonspacingMark | G::SpacingMark)
                || " -'&+".contains(c)
            {
                c
            } else {
                ' '
            }
        })
        .collect();
    let title = words(&kept)
        .map(|s| s.trim_matches(['-', '\'', '&', '+']))
        .filter(|s| !s.is_empty())
        .take(MAX_TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    let title: String = title.chars().take(MAX_GENERATED_TITLE_CHARS).collect();
    let title = title.trim_end_matches([' ', '-', '\'', '&', '+']);
    if !title.chars().any(|c| {
        matches!(
            cat(c),
            G::UppercaseLetter
                | G::LowercaseLetter
                | G::TitlecaseLetter
                | G::ModifierLetter
                | G::OtherLetter
        )
    }) || is_default_title(Some(title))
    {
        None
    } else {
        Some(title.into())
    }
}
pub fn title_prompt(transcript: &str, user_notes: &str) -> String {
    let mut data = serde_json::Map::new();
    data.insert(
        "notes".into(),
        serde_json::Value::String(
            trim(user_notes)
                .chars()
                .take(MAX_TITLE_NOTES_CHARS)
                .collect(),
        ),
    );
    data.insert(
        "transcript".into(),
        serde_json::Value::String(
            trim(transcript)
                .chars()
                .take(MAX_TITLE_TRANSCRIPT_CHARS)
                .collect(),
        ),
    );
    format!(
        "Meeting data:\n<<<\n{}\n>>>\nTitle:",
        neutralize(&py::json_dumps(
            &serde_json::Value::Object(data),
            None,
            false
        ))
    )
}

/// Owned snapshot for a detached title request. Plugins without a timeout use defaults.
pub trait TitleProvider: LlmProvider + Clone + Send + Sync + 'static {
    fn timeout(&self) -> Option<i64> {
        None
    }
    fn set_timeout(&mut self, _timeout: i64) {}
}
impl TitleProvider for OllamaLlm {
    fn timeout(&self) -> Option<i64> {
        Some(self.timeout)
    }
    fn set_timeout(&mut self, timeout: i64) {
        self.timeout = timeout;
    }
}
impl TitleProvider for OpenAiCompatLlm {
    fn timeout(&self) -> Option<i64> {
        Some(self.timeout)
    }
    fn set_timeout(&mut self, timeout: i64) {
        self.timeout = timeout;
    }
}
pub fn title_client<T: TitleProvider>(llm: Option<&T>, timeout: Duration) -> Option<Arc<T>> {
    llm.map(|llm| {
        let mut client = llm.clone();
        if let Some(current) = llm.timeout() {
            let timeout = timeout.as_secs_f64().ceil() as i64;
            client.set_timeout(if current > 0 {
                current.min(timeout)
            } else {
                timeout
            });
        }
        Arc::new(client)
    })
}
pub fn generate_title(
    llm: Option<Arc<dyn LlmProvider + Send + Sync>>,
    transcript: &str,
    user_notes: &str,
    started_at: NaiveDateTime,
    timeout: Duration,
) -> (String, bool) {
    let fallback = fallback_title(started_at);
    let Some(llm) = llm else {
        return (fallback, false);
    };
    if trim(transcript).is_empty() && trim(user_notes).is_empty() {
        return (fallback, false);
    }
    let prompt = title_prompt(transcript, user_notes);
    let (tx, rx) = mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("localflow-meeting-title".into())
        .spawn(move || {
            let result = llm.complete(
                crate::prompts::TITLE_SYSTEM,
                &prompt,
                Some(MAX_TITLE_TOKENS),
            );
            let _ = tx.send(result);
        })
        .is_err()
    {
        return (fallback, false);
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(reply)) => clean_generated_title(&reply)
            .map(|s| (s, true))
            .unwrap_or((fallback, false)),
        _ => (fallback, false),
    }
}
pub fn safe_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .filter(|c| py::is_alnum(*c) || " -_".contains(*c))
        .collect();
    let cleaned = trim(&cleaned);
    let mut end = cleaned.len().min(MAX_NAME_BYTES);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let value = trim(&cleaned[..end]);
    if value.is_empty() {
        DEFAULT_TITLE.into()
    } else {
        value.into()
    }
}
pub fn heading_text(title: &str) -> String {
    let text: String = title
        .nfkc()
        .map(|c| {
            if matches!(
                cat(c),
                G::Control
                    | G::Format
                    | G::PrivateUse
                    | G::Unassigned
                    | G::Surrogate
                    | G::SpaceSeparator
                    | G::LineSeparator
                    | G::ParagraphSeparator
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let text = words(&text).collect::<Vec<_>>().join(" ");
    let text = trim(text.trim_start_matches(['#', '>', '-', '*', '+', ' ']));
    let title: String = text.chars().take(MAX_GENERATED_TITLE_CHARS + 20).collect();
    if title.is_empty() {
        DEFAULT_TITLE.into()
    } else {
        title
    }
}
