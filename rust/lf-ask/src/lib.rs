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
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::LazyLock};

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
    /// Speaker label on a meeting passage's first line; only set when the
    /// caller cites speakers, and omitted from JSON (no key) when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}
/// Speaker-name cleaner (`lf_meetings::clean_speaker_name`) injected by callers
/// whose live `speaker_diarization` setting is on; injecting it avoids coupling
/// `lf-ask` to `lf-meetings`.
pub type SpeakerCleaner = fn(&str) -> Option<String>;
/// The `**Label:**` that follows a [`corpus::STAMP`] match; horizontal
/// whitespace only, so the separator can never cross into the next line. The
/// trailing `(?:[ \t]|\z)` stands in for Python's lookahead (only group 1 is used).
static SPEAKER_LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ \t]+\*\*([^*:\r\n]+):\*\*(?:[ \t]|\z)").unwrap());
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
/// Speaker label on the first line of a meeting passage, or `None`.
///
/// Only the first line counts (it is the line the citation's timestamp points
/// at). The label is returned exactly as written, and only when `clean` leaves
/// it unchanged (Me, Others, Speaker 2, an enrolled name, ...).
fn passage_speaker(passage: &Passage, clean: SpeakerCleaner) -> Option<String> {
    if passage.source != SOURCE_MEETING {
        return None;
    }
    let first = passage.text.split('\n').next().unwrap_or_default();
    let stamp = corpus::STAMP.find(first)?;
    let label = SPEAKER_LABEL
        .captures(&first[stamp.end()..])?
        .get(1)?
        .as_str();
    (clean(label).as_deref() == Some(label)).then(|| label.to_string())
}
pub fn citations(passages: &[Passage], question: &str) -> Vec<Citation> {
    cite(passages, question, None)
}
fn cite(passages: &[Passage], question: &str, speakers: Option<SpeakerCleaner>) -> Vec<Citation> {
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
            speaker: speakers.and_then(|clean| passage_speaker(p, clean)),
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
    search_with_speakers(question, corpus, options, None)
}
/// [`search`], adding `speaker` to meeting citations when `speakers` is set
/// (the caller's live `speaker_diarization` setting).
pub fn search_with_speakers(
    question: &str,
    corpus: &Corpus<'_>,
    options: &SearchOptions,
    speakers: Option<SpeakerCleaner>,
) -> Result<RecallResult> {
    let top = rank(question, &build_corpus(corpus)?, options)?;
    Ok(RecallResult {
        answer: None,
        citations: cite(&top, question, speakers),
    })
}
pub fn recall(
    question: &str,
    llm: &dyn LlmProvider,
    corpus: &Corpus<'_>,
    options: &SearchOptions,
) -> Result<RecallResult> {
    recall_with_speakers(question, llm, corpus, options, None)
}
/// [`recall`] with `speakers` as in [`search_with_speakers`]; the prompt is
/// unaffected.
pub fn recall_with_speakers(
    question: &str,
    llm: &dyn LlmProvider,
    corpus: &Corpus<'_>,
    options: &SearchOptions,
    speakers: Option<SpeakerCleaner>,
) -> Result<RecallResult> {
    let top = rank(question, &build_corpus(corpus)?, options)?;
    Ok(answer(question, llm, &top, speakers))
}
pub fn answer_from_passages(
    question: &str,
    llm: &dyn LlmProvider,
    passages: &[Passage],
) -> RecallResult {
    answer(question, llm, passages, None)
}
fn answer(
    question: &str,
    llm: &dyn LlmProvider,
    passages: &[Passage],
    speakers: Option<SpeakerCleaner>,
) -> RecallResult {
    if passages.is_empty() {
        return RecallResult {
            answer: Some(NO_ANSWER.into()),
            citations: vec![],
        };
    }
    let citations = cite(passages, question, speakers);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for `lf_meetings::clean_speaker_name`, so `lf-ask` stays
    /// uncoupled from `lf-meetings`: collapses whitespace and refuses names with no letter or
    /// digit. The real cleaner's Unicode rules are covered by the backend route
    /// test and the Python parity suite.
    fn clean(name: &str) -> Option<String> {
        let joined = name.split_whitespace().collect::<Vec<_>>().join(" ");
        joined.chars().any(char::is_alphanumeric).then_some(joined)
    }
    fn passage(text: &str, source: &str) -> Passage {
        Passage {
            text: text.into(),
            source: source.into(),
            title: "Standup".into(),
            ts: None,
            reference: "m.md#00:00".into(),
            app: String::new(),
        }
    }

    #[test]
    fn meeting_speaker_from_first_line_only() {
        for (text, speaker) in [
            ("**00:00** **Me:** we drop the beta", Some("Me")),
            ("**01:15** **Others:** agreed", Some("Others")),
            ("**02:30** **Speaker 2:** sounds good", Some("Speaker 2")),
            ("**03:45** **Dana O'Neil:** I own it", Some("Dana O'Neil")),
            ("**04:00** **José:** listo", Some("José")),
            (
                "**[05:10]** **Speaker 3:** bracketed stamp",
                Some("Speaker 3"),
            ),
            ("**1:02:03**\t**Me:** hour stamp, tab separator", Some("Me")),
            (
                "**00:00** **Me:** hi\n**00:10** **Dana:** the budget",
                Some("Me"),
            ),
            ("**00:00** budget\n**00:05** **Dana:** budget", None),
            ("**00:00** we drop the beta", None),
            ("**00:00**\n**Me:** hello", None),
            ("**00:00** said **Me:** hello", None),
            ("**Me:** no stamp at all", None),
            ("continuation of an over-long line **Me:** text", None),
            ("**00:00** **Dana  Smith:** needs rewriting", None),
            ("**00:00** **!!!:** junk", None),
            // Boundaries: label edges, separators and what may follow `:**`.
            ("**00:00** **Me:**", Some("Me")),
            ("**00:00**  \t **Me:**\tx", Some("Me")),
            ("**00:00** ** Me:** padded", None),
            ("**00:00** **Me :** padded", None),
            ("**00:00**\u{a0}**Me:** no-break space", None),
            ("**00:00****Me:** no separator", None),
            ("**00:00** **Me:**x glued", None),
            ("**00:00** **Me:**\r\nCRLF log", None),
            ("**00:00** **Me:**\u{a0}no-break space after", None),
            ("**00:00** **Me**: colon outside", None),
            ("**00:00** **Dana*Smith:** star", None),
        ] {
            let p = passage(text, SOURCE_MEETING);
            assert_eq!(passage_speaker(&p, clean).as_deref(), speaker, "{text:?}");
            let [on] = cite(std::slice::from_ref(&p), "budget", Some(clean))
                .try_into()
                .unwrap();
            let [off] = cite(std::slice::from_ref(&p), "budget", None)
                .try_into()
                .unwrap();
            assert_eq!(on.speaker.as_deref(), speaker, "{text:?}");
            assert_eq!(off.speaker, None, "{text:?}");
            assert_eq!(
                Citation {
                    speaker: None,
                    ..on
                },
                off,
                "{text:?}"
            );
        }
    }

    #[test]
    fn non_meeting_citations_never_get_speaker() {
        for source in [SOURCE_DICTATION, SOURCE_NOTES] {
            let passages = [passage("**00:00** **Me:** budget", source)];
            assert_eq!(
                cite(&passages, "budget", Some(clean)),
                citations(&passages, "budget"),
                "{source}"
            );
        }
    }

    #[test]
    fn speaker_key_is_last_and_absent_when_none() {
        let passages = [passage("**00:00** **Me:** budget", SOURCE_MEETING)];
        let keys = |c: &Citation| -> Vec<String> {
            match serde_json::to_value(c).unwrap() {
                serde_json::Value::Object(map) => map.keys().cloned().collect(),
                other => panic!("{other}"),
            }
        };
        let [off] = cite(&passages, "budget", None).try_into().unwrap();
        let [on] = cite(&passages, "budget", Some(clean)).try_into().unwrap();
        let base = ["n", "source", "title", "app", "when", "snippet", "ref"];
        assert_eq!(keys(&off), base);
        assert_eq!(keys(&on), [&base[..], &["speaker"][..]].concat());
        assert_eq!(serde_json::to_value(&on).unwrap()["speaker"], "Me");
        // Older payloads without the key still deserialize.
        let back: Citation = serde_json::from_value(serde_json::to_value(&off).unwrap()).unwrap();
        assert_eq!(back, off);
    }
}
