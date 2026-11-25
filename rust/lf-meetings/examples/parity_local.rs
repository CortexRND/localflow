//! JSON stdin/stdout driver for the isolated Python parity suite (no real services).
use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use lf_agent::{Change, ChatClient, SettingsAgent, Validator};
use lf_core::{config, history::HistoryStore};
use lf_meetings::{dispatch, titles, ObsidianWriter, Segment, Transcript, WriteNote};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    path::Path,
};

struct MockOllama(Value);
impl ChatClient for MockOllama {
    fn tags(&self, _: &str) -> Result<Value> {
        Ok(json!({"models":[{"name":"gemma4:e4b"}]}))
    }
    fn chat(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({"message":{"tool_calls":self.0}}))
    }
}
/// Records every completion call; replies "reply N" like the Python fake.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<Value>>);
impl lf_providers::LlmProvider for Recorder {
    fn id(&self) -> &'static str {
        "recorder"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, lf_providers::ProviderError> {
        let mut calls = self.0.lock().unwrap();
        calls.push(json!([system, user, max_tokens]));
        Ok(format!("reply {}", calls.len()))
    }
    fn list_models(&self) -> Result<Vec<lf_providers::ModelInfo>, lf_providers::ProviderError> {
        Ok(vec![])
    }
    fn healthcheck(&self) -> lf_providers::Health {
        lf_providers::Health::ok()
    }
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn date(v: &Value) -> Result<NaiveDateTime> {
    Ok(NaiveDateTime::parse_from_str(
        text(v, "now"),
        "%Y-%m-%dT%H:%M:%S",
    )?)
}
fn validator() -> Validator {
    Validator {
        platform: "darwin".into(),
        templates: Some(vec!["general".into(), "standup".into()]),
    }
}
fn run(v: &Value) -> Result<Value> {
    Ok(match text(v, "op") {
        "system_prompt" => json!(lf_agent::system_prompt(
            &config::load_from(Path::new(text(v, "config")))?,
            &validator()
        )),
        "request_message" => json!(lf_agent::request_message(text(v, "text"))),
        "validate" => validator().value(
            text(v, "key"),
            &v["value"],
            &config::load_from(Path::new(text(v, "config")))?,
        )?,
        "change" => {
            let change: Change = serde_json::from_value(v["change"].clone())?;
            json!({"line":change.line(),"warning":change.warning_for(text(v,"request")).unwrap_or_default()})
        }
        "propose" => {
            let root = Path::new(text(v, "root"));
            let mut agent = SettingsAgent::new(
                MockOllama(v["calls"].clone()),
                text(v, "config").into(),
                root.join("dictionary.json"),
                root.join("undo.json"),
            );
            agent.validator = validator();
            let proposal = agent.propose(text(v, "request"))?;
            json!({"lines":proposal.changes.iter().map(Change::line).collect::<Vec<_>>(),"warnings":proposal.warnings(),"rejected":proposal.rejected,"model":proposal.model})
        }
        "ask_text" => {
            let since = v["since"].as_str().map(lf_ask::Since::from);
            json!({"tokens":lf_ask::tokenize(text(v,"text"),false),"stems":v["words"].as_array().context("words")?.iter().map(|s|lf_ask::stem(s.as_str().unwrap())).collect::<Vec<_>>(),"chunks":lf_ask::chunk_lines(text(v,"text")),"since":lf_ask::parse_since(since.as_ref(),date(v)?)?.map(|d|d.format("%Y-%m-%dT%H:%M:%S").to_string())})
        }
        "search" => {
            let store = HistoryStore::open(Some(text(v, "db").into()), None, true)?;
            let corpus = lf_ask::Corpus {
                store: Some(&store),
                vault_path: text(v, "vault").into(),
                notes_folder: "Notes".into(),
                logs_folder: "Transcripts".into(),
            };
            let options = lf_ask::SearchOptions {
                now: date(v)?,
                since: v["since"].as_str().map(lf_ask::Since::from),
                ..Default::default()
            };
            lf_ask::search(text(v, "question"), &corpus, &options)?.to_dict()
        }
        "meetings" => {
            let segments: Vec<Segment> = serde_json::from_value(v["segments"].clone())?;
            json!({"transcript":lf_meetings::format_transcript(&segments),"media":lf_meetings::strip_remote_media(text(v,"text")),"clean":titles::clean_generated_title(text(v,"title")),"safe":titles::safe_title(text(v,"title")),"fallback":titles::fallback_title(date(v)?),"prompts":dispatch::parse_work_prompts(text(v,"prompts")),"slug":dispatch::slugify(text(v,"title"))})
        }
        "speakers" => {
            let segments: Vec<Segment> = serde_json::from_value(v["segments"].clone())?;
            let transcript = lf_meetings::format_transcript(&segments);
            let diarized = lf_meetings::is_diarized(&segments);
            let chunk = v["chunk"].as_u64().context("chunk")? as usize;
            let llm = Recorder::default();
            let summarizer = lf_meetings::MeetingSummarizer {
                llm: &llm,
                words_per_chunk: chunk,
            };
            let marked = Transcript::new(&transcript, diarized);
            for user_notes in ["", "## Pricing\n- price?"] {
                summarizer.summarize(marked, user_notes, None);
            }
            for words_per_chunk in [4000, chunk] {
                lf_meetings::answer_question(&llm, marked, "Who spoke?", words_per_chunk)?;
            }
            let names = v["names"].as_array().context("names")?;
            json!({"transcript":transcript,"diarized":diarized,"labels":segments.iter().map(|s|s.speaker()).collect::<Vec<_>>(),"clean":names.iter().map(|s|lf_meetings::clean_speaker_name(s.as_str().unwrap_or(""))).collect::<Vec<_>>(),"calls":*llm.0.lock().unwrap()})
        }
        "write" => {
            let root = Path::new(text(v, "vault"));
            let writer = ObsidianWriter::new(root, Path::new("Notes"), Path::new("Transcripts"));
            let segments: Vec<Segment> = serde_json::from_value(v["segments"].clone())?;
            let note = writer.write(&WriteNote {
                title: text(v, "title"),
                category: "Zoom",
                started_at: date(v)?,
                duration_seconds: 125.0,
                notes_md: text(v, "notes"),
                segments: &segments,
                user_notes: "- follow up",
                template: "general",
                generated_title: false,
            })?;
            json!({"notes_path":note.notes_path.strip_prefix(root)?.to_string_lossy(),"log_path":note.log_path.strip_prefix(root)?.to_string_lossy(),"notes":fs::read(note.notes_path)?,"log":fs::read(note.log_path)?})
        }
        "queue" => {
            let queue = dispatch::PromptQueue::new(text(v, "path").into());
            match text(v, "action") {
                "add" => {
                    let prompts = dispatch::parse_work_prompts(text(v, "prompts"));
                    let paths = (0..prompts.len())
                        .map(|i| Path::new(text(v, "root")).join(format!("prompt-{i}.md")))
                        .collect::<Vec<_>>();
                    json!(queue.add_at(&prompts, &paths, "Planning", date(v)?)?)
                }
                "status" => json!(queue.set_status(
                    text(v, "id"),
                    text(v, "status"),
                    None,
                    &serde_json::Map::new()
                )?),
                _ => json!(queue.list(None)?),
            }
        }
        op => anyhow::bail!("unknown operation {op}"),
    })
}
fn main() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let requests: Vec<Value> = serde_json::from_str(&input)?;
    let responses: Vec<_> = requests
        .iter()
        .map(|v| match run(v) {
            Ok(value) => json!({"ok":value}),
            Err(error) => json!({"error":error.to_string()}),
        })
        .collect();
    println!("{}", serde_json::to_string(&responses)?);
    Ok(())
}
