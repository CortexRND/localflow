//! Local recall over dictations, meeting transcripts, and notes.
mod corpus;
mod ranking;
mod text;
pub use corpus::*;
pub use ranking::*;
pub use text::*;

use anyhow::Result;
use chrono::{DateTime, Local};
use lf_providers::LlmProvider;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const NO_ANSWER: &str = "I couldn't find that in your history.";
pub const SOURCE_DICTATION: &str = "dictation";
pub const SOURCE_MEETING: &str = "meeting";
pub const SOURCE_NOTES: &str = "notes";
pub const PROMPT_PASSAGE_MAX_WORDS: usize = 300;
pub const SNIPPET_CHARS: usize = 200;
pub const HISTORY_SCAN_LIMIT: i64 = 200_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Passage {
    pub text: String,
    pub source: String,
    pub title: String,
    pub ts: Option<f64>,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub app: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub n: usize,
    pub source: String,
    pub title: String,
    pub app: String,
    pub when: String,
    pub snippet: String,
    #[serde(rename = "ref")]
    pub reference: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    pub answer: Option<String>,
    pub citations: Vec<Citation>,
}
impl RecallResult {
    pub fn to_dict(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("serializable recall result")
    }
}
/// Convert a Unix timestamp to local time at Python datetime precision.
pub fn local_time(ts: f64) -> Option<DateTime<Local>> {
    if !ts.is_finite() {
        return None;
    }
    let mut secs = ts.floor() as i64;
    let mut micros = ((ts - secs as f64) * 1_000_000.0).round_ties_even() as u32;
    if micros == 1_000_000 {
        secs = secs.checked_add(1)?;
        micros = 0;
    }
    DateTime::from_timestamp(secs, micros * 1_000).map(|d| d.with_timezone(&Local))
}
pub fn citations(passages: &[Passage], question: &str) -> Vec<Citation> {
    let (plain, _) = extract_phrases(question);
    let terms: HashSet<_> = tokenize(&plain, false).into_iter().collect();
    passages
        .iter()
        .enumerate()
        .map(|(i, p)| Citation {
            n: i + 1,
            source: p.source.clone(),
            title: p.title.clone(),
            app: p.app.clone(),
            when: p
                .ts
                .and_then(local_time)
                .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
                .unwrap_or_default(),
            snippet: snippet(&p.text, &terms),
            reference: p.reference.clone(),
        })
        .collect()
}
pub fn format_passage(n: usize, passage: &Passage) -> String {
    let when = passage
        .ts
        .and_then(local_time)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".into());
    let label = if passage.title.is_empty() {
        passage.source.clone()
    } else {
        format!("{} · {}", passage.source, passage.title)
    };
    let words: Vec<_> = text::words(&passage.text).collect();
    let text = if words.len() > PROMPT_PASSAGE_MAX_WORDS {
        format!("{} …", words[..PROMPT_PASSAGE_MAX_WORDS].join(" "))
    } else {
        passage.text.clone()
    };
    format!("[{n}] ({when} · {label})\n<<<\n{text}\n>>>")
}
pub fn build_prompt(question: &str, passages: &[Passage]) -> String {
    let passages = passages
        .iter()
        .enumerate()
        .map(|(i, p)| format_passage(i + 1, p))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("You answer questions about the user's own past dictations, meetings and notes. Use ONLY the numbered passages below. Be concise (a few sentences). Cite the passages you used like [2]. Never guess or add outside knowledge. If the passages do not contain the answer, reply with exactly: {NO_ANSWER}\n\nPassages:\n{passages}\n\nQuestion: {question}\n")
}
pub fn search(
    question: &str,
    corpus: &Corpus<'_>,
    options: &SearchOptions,
) -> Result<RecallResult> {
    let top = rank(question, &build_corpus(corpus)?, options)?;
    Ok(RecallResult {
        answer: None,
        citations: citations(&top, question),
    })
}
pub fn recall(
    question: &str,
    llm: &dyn LlmProvider,
    corpus: &Corpus<'_>,
    options: &SearchOptions,
) -> Result<RecallResult> {
    let top = rank(question, &build_corpus(corpus)?, options)?;
    Ok(answer_from_passages(question, llm, &top))
}
pub fn answer_from_passages(
    question: &str,
    llm: &dyn LlmProvider,
    passages: &[Passage],
) -> RecallResult {
    if passages.is_empty() {
        return RecallResult {
            answer: Some(NO_ANSWER.into()),
            citations: vec![],
        };
    }
    let citations = citations(passages, question);
    match llm.complete("", &build_prompt(question, passages), None) {
        Err(_) => RecallResult {
            answer: None,
            citations,
        },
        Ok(reply) => {
            let reply = reply.trim_matches(text::python_whitespace);
            if reply.is_empty()
                || reply
                    .to_lowercase()
                    .contains(NO_ANSWER.to_lowercase().trim_end_matches('.'))
            {
                RecallResult {
                    answer: Some(NO_ANSWER.into()),
                    citations: vec![],
                }
            } else {
                RecallResult {
                    answer: Some(clean_citations(reply, passages.len())),
                    citations,
                }
            }
        }
    }
}
