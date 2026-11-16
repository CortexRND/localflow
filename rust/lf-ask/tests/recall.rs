use chrono::{Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use lf_ask::*;
use lf_core::{
    config::Config,
    history::{HistoryStore, NewEntry},
};
use lf_providers::{Health, LlmProvider, ModelInfo, ProviderError};
use serde_json::json;
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}
fn ts(d: NaiveDateTime) -> f64 {
    Local
        .from_local_datetime(&d)
        .earliest()
        .unwrap()
        .timestamp() as f64
}
fn options() -> SearchOptions {
    SearchOptions {
        now: now(),
        ..Default::default()
    }
}
fn p(text: &str, days: i64, reference: &str) -> Passage {
    Passage {
        text: text.into(),
        source: SOURCE_DICTATION.into(),
        title: String::new(),
        ts: Some(ts(now() - Duration::days(days))),
        reference: reference.into(),
        app: String::new(),
    }
}
struct Fake {
    reply: Result<String, ProviderError>,
    prompts: RefCell<Vec<String>>,
}
impl Fake {
    fn new(reply: &str) -> Self {
        Self {
            reply: Ok(reply.into()),
            prompts: RefCell::new(vec![]),
        }
    }
}
impl LlmProvider for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError> {
        assert_eq!(system, "");
        assert_eq!(max_tokens, None);
        self.prompts.borrow_mut().push(user.into());
        self.reply.clone()
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![])
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}
fn store(root: &Path) -> HistoryStore {
    let store = HistoryStore::open(
        Some(root.join("history.sqlite")),
        Some(root.join("clips")),
        true,
    )
    .unwrap();
    for (text, app, status, seconds) in [
        (
            "Sarah the budget is five thousand dollars",
            "Slack",
            "ok",
            3600,
        ),
        ("remember to buy milk and eggs", "Notes", "ok", 7200),
        ("failed one about budget", "", "failed", 100),
        ("", "", "ok", 50),
    ] {
        let mut entry = NewEntry::new("dictate");
        entry.final_text = text.into();
        entry.app_name = Some(app.into());
        entry.status = status.into();
        entry.ts = Some(ts(now()) - seconds as f64);
        store.add(&entry).unwrap();
    }
    store
}
fn vault(root: &Path) -> (PathBuf, PathBuf) {
    let vault = root.join("vault");
    fs::create_dir_all(vault.join("notes/nested")).unwrap();
    fs::create_dir_all(vault.join("logs/nested")).unwrap();
    let log = vault
        .join("logs")
        .join("nested")
        .join("2026-11-14 1000 Launch sync (transcript).md");
    fs::write(&log,"---\ncategory: Zoom\n---\n# Launch sync — transcript\n\nNotes: [[Launch sync]]\n\n**00:00** **Me:** we will drop the Windows beta next week\n\n**01:15** **Others:** agreed, Dana owns the announcement\n\n## My raw notes\nPRIVATE RAW NOTES").unwrap();
    fs::write(
        vault.join("notes/nested/2026-11-14 1000 Launch sync.md"),
        "---\ncategory: Zoom\n---\n## Summary\nWe agreed to drop the Windows beta.",
    )
    .unwrap();
    (vault, log)
}
#[test]
fn bm25_right_passage_and_empty_stopword_queries() {
    let passages=vec![p("lunch plans for friday with the team",1,"a"),p("the quarterly budget review is moved to thursday",1,"b"),p("budget budget budget but also a lot of other unrelated filler words here that dilute the passage considerably so it should rank lower overall",1,"c")];
    let top = rank("when is the budget review", &passages, &options()).unwrap();
    assert_eq!(top[0].reference, "b");
    assert!(!top.iter().any(|p| p.reference == "a"));
    for question in ["what is the", "", "zebra migration"] {
        assert!(rank(question, &passages, &options()).unwrap().is_empty());
    }
    assert!(rank("budget", &[], &options()).unwrap().is_empty());
    let mut opts = options();
    opts.k = 0;
    assert!(rank("budget", &passages, &opts).unwrap().is_empty());
}
#[test]
fn stemming_tokenizer_stopwords_and_non_latin() {
    for (input, expected) in [
        ("budgets", "budget"),
        ("budget", "budget"),
        ("dropping", "drop"),
        ("dropped", "drop"),
        ("policies", "policy"),
        ("business", "business"),
        ("classes", "class"),
        ("status", "status"),
        ("analysis", "analysis"),
        ("boss's", "boss"),
        ("don't", "dont"),
    ] {
        assert_eq!(stem(input), expected);
    }
    assert_eq!(
        tokenize("What did we say about the budget?", false),
        vec!["budget"]
    );
    assert_eq!(tokenize("Привет мир", false), tokenize("привет мир", false));
    assert!(!tokenize("Привет мир", false).is_empty());
    assert_eq!(
        tokenize("a_b café e\u{301} 日本語 123", true),
        vec!["a", "b", "café", "e", "日本語", "123"]
    );
    assert_eq!(
        rank(
            "budget",
            &[p("we reviewed the budgets today", 1, "1")],
            &options()
        )
        .unwrap()
        .len(),
        1
    );
}
#[test]
fn phrase_boost_recency_and_stable_ties() {
    let passages = vec![
        p("review the plan and then the budget later", 1, "scattered"),
        p("the budget review is on thursday", 1, "exact"),
    ];
    let top = rank("\"budget review\"", &passages, &options()).unwrap();
    assert_eq!(top[0].reference, "exact");
    assert_eq!(top.len(), 2);
    assert_eq!(
        rank(
            "budget",
            &[
                p("budget approved", 90, "old"),
                p("budget approved", 1, "new")
            ],
            &options()
        )
        .unwrap()[0]
            .reference,
        "new"
    );
    let tied = [
        p("budget approved", 1, "first"),
        p("budget approved", 1, "second"),
    ];
    assert_eq!(
        rank("budget", &tied, &options()).unwrap()[0].reference,
        "first"
    );
    assert_eq!(
        extract_phrases("find \"the budget\""),
        (
            "find  the budget ".into(),
            vec![vec!["the".into(), "budget".into()]]
        )
    );
}
#[test]
fn since_parsing_dates_and_filters() {
    let today = now().date().and_hms_opt(0, 0, 0).unwrap();
    for (input, expected) in [
        ("today", today),
        ("yesterday", today - Duration::days(1)),
        ("7d", now() - Duration::days(7)),
        (" 7 D ", now() - Duration::days(7)),
        (
            "2026-11-01",
            NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        ),
    ] {
        assert_eq!(
            parse_since(Some(&Since::from(input)), now()).unwrap(),
            Some(expected)
        );
    }
    assert!(parse_since(None, now()).unwrap().is_none());
    assert!(parse_since(Some(&Since::from("")), now())
        .unwrap()
        .is_none());
    assert_eq!(
        parse_since(Some(&Since::from(today.date())), now()).unwrap(),
        Some(today)
    );
    assert_eq!(
        parse_since(Some(&Since::from(now())), now()).unwrap(),
        Some(now())
    );
    for bad in [
        "last week",
        "soon",
        "2026-02-30",
        " ",
        "9999999999999999999999999999d",
    ] {
        assert!(parse_since(Some(&Since::from(bad)), now()).is_err());
    }
    let mut unknown = p("budget unknown", 0, "unknown");
    unknown.ts = None;
    let passages = vec![
        p("budget old", 40, "old"),
        p("budget new", 2, "new"),
        unknown,
    ];
    let mut opts = options();
    opts.since = Some("30d".into());
    assert_eq!(
        rank("budget", &passages, &opts)
            .unwrap()
            .iter()
            .map(|p| p.reference.as_str())
            .collect::<Vec<_>>(),
        vec!["new"]
    );
}
#[test]
fn filename_stamp_mtime_and_fallback_title() {
    let dir = tempfile::tempdir().unwrap();
    let (when, title) = parse_file_stamp(
        &dir.path()
            .join("2026-11-14 1030 Launch sync (transcript).md"),
    );
    assert_eq!(
        when,
        NaiveDate::from_ymd_opt(2026, 11, 14)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
    );
    assert_eq!(title, "Launch sync");
    let odd = dir.path().join("random notes.md");
    fs::write(&odd, "x").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&odd)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        )
        .unwrap();
    let (when, title) = parse_file_stamp(&odd);
    assert_eq!(ts(when), 1_700_000_000.0);
    assert_eq!(title, "random notes");
    assert_eq!(
        parse_file_stamp(&dir.path().join("2026-11-14 1030.md")).1,
        "2026-11-14 1030"
    );
    assert_eq!(
        parse_file_stamp(&dir.path().join("2026-99-14 1030 bad.md")).1,
        "2026-99-14 1030 bad"
    );
}
#[test]
fn chunks_preserve_speaker_lines_and_split_long_lines() {
    let lines: Vec<_> = (0..10)
        .map(|i| {
            format!(
                "**{i:02}:00** **Me:** {}",
                (0..40)
                    .map(|j| format!("w{i}x{j}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .collect();
    let chunks = chunk_lines(&lines.join("\n\n"));
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| c.split_whitespace().count() <= 200));
    assert_eq!(chunks.join("\n").matches("**Me:**").count(), 10);
    for chunk in chunks {
        for line in chunk.lines() {
            assert!(line.starts_with("**") && line.contains("**Me:**"));
        }
    }
    let chunks = chunk_lines(&format!(
        "**00:00** **Me:** {}",
        vec!["word"; 450].join(" ")
    ));
    assert_eq!(chunks.len(), 3);
    assert!(chunks[0].starts_with("**00:00**"));
    assert!(chunk_lines(" \n\t ").is_empty());
}
#[test]
fn transcripts_refs_times_frontmatter_and_bad_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, log) = vault(dir.path());
    let mixed = vault.join("logs/nested/2026-11-14 1000 Launch sync (transcript).md");
    let passages = transcript_passages(&mixed);
    assert_eq!(passages.len(), 1);
    let first = &passages[0];
    assert_eq!(first.source, "meeting");
    assert_eq!(first.title, "Launch sync");
    assert_eq!(first.reference, format!("{}#00:00", log.display()));
    let notes = vault
        .join("notes")
        .join("nested")
        .join("2026-11-14 1000 Launch sync.md");
    let mixed_notes = vault.join("notes/nested/2026-11-14 1000 Launch sync.md");
    assert_eq!(
        notes_passages(&mixed_notes)[0].reference,
        notes.to_string_lossy()
    );
    assert!(first.text.contains("Windows beta"));
    assert!(!first.text.contains("Notes: [["));
    assert!(!first.text.contains("PRIVATE RAW"));
    assert!(!first.text.contains("category: Zoom"));
    assert!(!first.text.contains("# Launch"));
    assert_eq!(
        first.ts,
        Some(ts(NaiveDate::from_ymd_opt(2026, 11, 14)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap()))
    );
    fs::write(&log, b"**00:01** **Me:** caf\xe9 \xff\xfe").unwrap();
    assert!(transcript_passages(&log).is_empty());
    assert!(notes_passages(&log).is_empty());
    assert!(transcript_passages(&dir.path().join("missing.md")).is_empty());
}
#[test]
fn transcript_bracketed_hour_stamp_and_chunk_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("2026-11-14 1000 Sync (transcript).md");
    fs::write(
        &file,
        format!(
            "**[01:02:03]** Me: {}\n**[02:03:04]** Others: next",
            vec!["word"; 150].join(" ")
        ),
    )
    .unwrap();
    let passages = transcript_passages(&file);
    assert_eq!(passages.len(), 2);
    let start = NaiveDate::from_ymd_opt(2026, 11, 14)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    assert_eq!(passages[0].ts, Some(ts(start + Duration::seconds(3723))));
    assert_eq!(passages[1].ts, Some(ts(start + Duration::seconds(7384))));
}
#[test]
fn search_dictation_meetings_notes_from_config_and_sources() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let (vault, log) = vault(dir.path());
    let mut config = Config::default();
    config.set_field("vault_path", json!(vault));
    config.set_field("notes_folder", json!("notes"));
    config.set_field("logs_folder", json!("logs"));
    let corpus = Corpus::from_config(&config, Some(&store));
    let result = search("windows beta", &corpus, &options()).unwrap();
    assert_eq!(result.answer, None);
    assert_eq!(result.citations.len(), 2);
    assert!(result
        .citations
        .iter()
        .all(|c| ["meeting", "notes"].contains(&c.source.as_str())));
    let meeting = result
        .citations
        .iter()
        .find(|c| c.source == "meeting")
        .unwrap();
    assert_eq!(meeting.reference, format!("{}#00:00", log.display()));
    assert_eq!(meeting.when, "2026-11-14T10:00");
    let result = search("budget", &corpus, &options()).unwrap();
    assert_eq!(result.citations.len(), 1);
    let c = &result.citations[0];
    assert_eq!(c.app, "Slack");
    assert_eq!(c.reference, "1");
    assert_eq!(c.n, 1);
    assert_eq!(result.to_dict()["citations"][0]["ref"], "1");
    let mut opts = options();
    opts.sources = vec!["notes".into()];
    assert_eq!(
        search("windows beta", &corpus, &opts).unwrap().citations[0].source,
        "notes"
    );
    opts.sources = vec!["unknown".into()];
    assert!(search("windows beta", &corpus, &opts)
        .unwrap()
        .citations
        .is_empty());
}
#[test]
fn llm_answer_citation_cleaning_and_exact_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let corpus = Corpus {
        store: Some(&store),
        ..Default::default()
    };
    let llm = Fake::new("The budget is five thousand [1]. Also [7] and [1, 9].");
    let result = recall("what is the budget", &llm, &corpus, &options()).unwrap();
    assert_eq!(
        result.answer.as_deref(),
        Some("The budget is five thousand [1]. Also and [1].")
    );
    assert_eq!(result.citations.len(), 1);
    let top = rank(
        "what is the budget",
        &build_corpus(&corpus).unwrap(),
        &options(),
    )
    .unwrap();
    assert_eq!(
        llm.prompts.borrow()[0],
        build_prompt("what is the budget", &top)
    );
    assert_eq!(build_prompt("budget?",&[]),format!("You answer questions about the user's own past dictations, meetings and notes. Use ONLY the numbered passages below. Be concise (a few sentences). Cite the passages you used like [2]. Never guess or add outside knowledge. If the passages do not contain the answer, reply with exactly: {NO_ANSWER}\n\nPassages:\n\n\nQuestion: budget?\n"));
    assert!(llm.prompts.borrow()[0].contains("Slack"));
    assert!(llm.prompts.borrow()[0].contains("<<<\nSarah the budget"));
}
#[test]
fn clean_citations_preserves_timestamps_and_normalizes_punctuation() {
    for (input, valid, expected) in [
        ("said at [12:30] [3]", 2, "said at [12:30]"),
        ("A [1; 2, 9] .", 2, "A [1, 2]."),
        ("[0] [99]", 1, ""),
        ("Keep [123] and [1]", 1, "Keep [123] and [1]"),
        ("[1, 1]", 1, "[1, 1]"),
    ] {
        assert_eq!(clean_citations(input, valid), expected);
    }
}
#[test]
fn no_answer_empty_sentinel_and_provider_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let corpus = Corpus {
        store: Some(&store),
        ..Default::default()
    };
    let llm = Fake::new("unused");
    let result = recall("zebra migration", &llm, &corpus, &options()).unwrap();
    assert_eq!(result.answer.as_deref(), Some(NO_ANSWER));
    assert!(result.citations.is_empty());
    assert!(llm.prompts.borrow().is_empty());
    for reply in [
        NO_ANSWER,
        "",
        " \n ",
        "I COULDN'T FIND THAT IN YOUR HISTORY. [1]",
    ] {
        let result = recall("budget", &Fake::new(reply), &corpus, &options()).unwrap();
        assert_eq!(result.answer.as_deref(), Some(NO_ANSWER));
        assert!(result.citations.is_empty());
    }
    let llm = Fake {
        reply: Err(ProviderError("down".into())),
        prompts: RefCell::new(vec![]),
    };
    let result = recall("budget", &llm, &corpus, &options()).unwrap();
    assert_eq!(result.answer, None);
    assert_eq!(result.citations.len(), 1);
}
#[test]
fn history_disabled_missing_folders_and_config_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let off = HistoryStore::open(Some(dir.path().join("off.sqlite")), None, false).unwrap();
    assert!(search(
        "budget",
        &Corpus {
            store: Some(&off),
            ..Default::default()
        },
        &options()
    )
    .unwrap()
    .citations
    .is_empty());
    assert!(search("budget", &Corpus::default(), &options())
        .unwrap()
        .citations
        .is_empty());
    let store = store(dir.path());
    let mut config = Config::default();
    config.set_field("history_enabled", json!(false));
    config.set_field("vault_path", json!(dir.path().join("missing")));
    assert!(search(
        "budget",
        &Corpus::from_config(&config, Some(&store)),
        &options()
    )
    .unwrap()
    .citations
    .is_empty());
}
#[test]
fn long_dictation_late_facts_prompt_limit_and_unicode_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let store = HistoryStore::open(Some(dir.path().join("history.sqlite")), None, true).unwrap();
    let mut entry = NewEntry::new("dictate");
    entry.final_text = format!(
        "{} the gate code is 4417",
        (0..400)
            .map(|i| format!("filler{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    entry.ts = Some(ts(now()));
    store.add(&entry).unwrap();
    let corpus = Corpus {
        store: Some(&store),
        ..Default::default()
    };
    let llm = Fake::new("4417 [1]");
    recall("gate code", &llm, &corpus, &options()).unwrap();
    assert!(llm.prompts.borrow()[0].contains("4417"));
    assert!(!llm.prompts.borrow()[0].contains("filler0 "));
    let mut passage = p(&vec!["word"; 400].join(" "), 0, "1");
    passage.ts = None;
    assert!(format_passage(1, &passage).contains("unknown time"));
    assert!(format_passage(1, &passage).contains(" …\n>>>"));
    let text = format!("{} budget {}", "日".repeat(300), "語".repeat(300));
    let snip = snippet(&text, &["budget".into()].into_iter().collect());
    assert!(snip.contains("budget"));
    assert!(snip.starts_with('…') && snip.ends_with('…'));
    assert!(snip.chars().count() <= 202);
}

#[test]
fn frozen_python_reference_strings_retrieval_and_edge_cases() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("python-reference.json")).unwrap();
    let passages: Vec<Passage> = serde_json::from_value(fixture["passages"].clone()).unwrap();
    assert_eq!(build_prompt("What budget?", &passages), fixture["prompt"]);
    assert_eq!(json!(citations(&passages, "budget")), fixture["citations"]);
    assert_eq!(
        json!(rank("budget \"budget plan\"", &passages, &options())
            .unwrap()
            .iter()
            .map(|p| &p.reference)
            .collect::<Vec<_>>()),
        fixture["ranked"]
    );
    for case in fixture["texts"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(json!(tokenize(text, false)), case["tokens"]);
        assert_eq!(json!(chunk_lines(text)), case["chunks"]);
        assert_eq!(
            snippet(text, &["budget".into()].into_iter().collect()),
            case["snippet"]
        );
    }
    for case in fixture["since"].as_array().unwrap() {
        let result = parse_since(Some(&Since::from(case["value"].as_str().unwrap())), now());
        if let Some(error) = case.get("error") {
            assert_eq!(result.unwrap_err().to_string(), error.as_str().unwrap());
        } else {
            assert_eq!(
                json!(result
                    .unwrap()
                    .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())),
                case["ok"]
            );
        }
    }
    for case in fixture["clean"].as_array().unwrap() {
        assert_eq!(
            clean_citations(case["text"].as_str().unwrap(), 3),
            case["result"]
        );
    }
}

#[test]
fn python_microsecond_rounding_and_zero_timestamp_ties() {
    let mut first = p("budget", 0, "first");
    first.ts = Some(-0.0);
    let mut second = first.clone();
    second.ts = Some(0.0);
    second.reference = "second".into();
    let top = rank("budget", &[first.clone(), second], &options()).unwrap();
    assert_eq!(top[0].reference, "first");
    first.ts = Some(ts(now()) - 0.0000002);
    assert_eq!(
        citations(&[first.clone()], "budget")[0].when,
        "2026-11-15T12:00"
    );
    assert!(format_passage(1, &first).starts_with("[1] (2026-11-15 12:00"));
    assert!(std::panic::catch_unwind(|| chunk_lines_with_limits("word", 10, 0)).is_err());
    assert!(chunk_lines_with_limits("", 10, 0).is_empty());
}
#[test]
fn corpus_skips_transcripts_in_notes_and_reads_newline_formats() {
    let dir = tempfile::tempdir().unwrap();
    let (vault, log) = vault(dir.path());
    let corpus = Corpus {
        vault_path: vault.clone(),
        notes_folder: "notes".into(),
        logs_folder: "logs".into(),
        store: None,
    };
    fs::copy(
        &log,
        vault.join("notes/2026-11-14 1000 Duplicate (transcript).md"),
    )
    .unwrap();
    fs::write(vault.join("notes/invalid.md"), [0xff]).unwrap();
    assert_eq!(build_corpus(&corpus).unwrap().len(), 2);
    fs::write(&log,"---\r\nsecret: value\r\n---\r\n# heading\r\n**00:00** Me: budget\r\nNotes: [[x]]\r\n## My raw notes\r\nprivate").unwrap();
    let p = transcript_passages(&log);
    assert_eq!(p[0].text, "**00:00** Me: budget");
}
