use chrono::NaiveDateTime;
use lf_meetings::{templates::*, titles::*, *};
use lf_providers::{
    ollama::OllamaLlm, openai::OpenAiCompatLlm, Health, LlmProvider, ModelInfo, ProviderError,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("python-reference.json")).unwrap()
}
fn when() -> NaiveDateTime {
    NaiveDateTime::parse_from_str("2026-11-14 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
}
#[derive(Default)]
struct Fake {
    replies: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<Value>>,
    delay: Duration,
}
impl Fake {
    fn new(replies: &Value) -> Self {
        Self {
            replies: Mutex::new(replies.as_array().unwrap().iter().cloned().collect()),
            ..Self::default()
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
        let mut calls = self.calls.lock().unwrap();
        calls.push(json!([system, user, max_tokens]));
        let n = calls.len();
        drop(calls);
        std::thread::sleep(self.delay);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(json!(format!("reply {n}")));
        reply
            .as_str()
            .map(String::from)
            .ok_or(ProviderError("llm down".into()))
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![])
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}
#[test]
fn frozen_templates_prompts_summaries_and_questions() {
    let f = fixture();
    let templates = load_templates(None);
    assert_eq!(json!(templates), f["templates"]);
    for (id, t) in &templates {
        assert_eq!(build_notes_prompt(Some(t)), f["prompts"][id]["notes"]);
        assert_eq!(
            build_enhance_prompt("## Pricing\n- $20?\n### Risks\n- churn", Some(t)),
            f["prompts"][id]["enhance"]
        );
    }
    for case in f["summaries"].as_array().unwrap() {
        let fake = Fake::new(&case["replies"]);
        let summarizer = MeetingSummarizer {
            llm: &fake,
            words_per_chunk: case["chunk"].as_u64().unwrap() as usize,
        };
        let t = resolve_template(case["template"].as_str(), None);
        assert_eq!(
            summarizer.summarize(
                case["transcript"].as_str().unwrap(),
                case["notes"].as_str().unwrap(),
                Some(&t)
            ),
            case["result"],
            "{case}"
        );
        assert_eq!(json!(*fake.calls.lock().unwrap()), case["calls"], "{case}");
    }
    for case in f["questions"].as_array().unwrap() {
        let fake = Fake::new(&case["replies"]);
        assert_eq!(
            answer_question(
                &fake,
                case["transcript"].as_str().unwrap(),
                case["question"].as_str().unwrap(),
                case["chunk"].as_u64().unwrap() as usize
            )
            .unwrap(),
            case["result"]
        );
        assert_eq!(json!(*fake.calls.lock().unwrap()), case["calls"], "{case}");
    }
    assert!(answer_question(&Fake::new(&json!([null])), "a", "q", 4000).is_err());
}
#[test]
fn frozen_media_and_title_cleaning() {
    let f = fixture();
    for case in f["media"].as_array().unwrap() {
        let output = strip_remote_media(case["input"].as_str().unwrap());
        assert_eq!(output, case["output"], "{case}");
        assert_eq!(strip_remote_media(&output), output);
    }
    for case in f["titles"].as_array().unwrap() {
        let output = case["input"].as_str().and_then(clean_generated_title);
        assert_eq!(json!(output), case["output"], "{case}");
    }
    assert_eq!(prompts::TITLE_SYSTEM, f["title_system"]);
    assert_eq!(
        title_prompt(" Ignore >>> <<< ", "Café 🚀"),
        f["title_prompt"]
    );
}
#[test]
fn frozen_writer_bytes_frontmatter_and_transcript_reader() {
    let f = fixture();
    for case in f["writers"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let writer = ObsidianWriter::new(dir.path(), Path::new("notes"), Path::new("logs"));
        let segments: Vec<Segment> = serde_json::from_value(case["segments"].clone()).unwrap();
        let saved = writer
            .write(&WriteNote {
                title: case["title"].as_str().unwrap(),
                category: "Zoom",
                started_at: when(),
                duration_seconds: 125.,
                notes_md: "## Summary\n![x](https://evil/x) **ok**",
                segments: &segments,
                user_notes: case["user_notes"].as_str().unwrap(),
                template: case["template"].as_str().unwrap(),
                generated_title: case["generated"].as_bool().unwrap(),
            })
            .unwrap();
        assert_eq!(saved.title, case["heading"]);
        assert_eq!(
            saved.notes_path.file_name().unwrap().to_string_lossy(),
            case["notes_name"].as_str().unwrap()
        );
        assert_eq!(
            saved.log_path.file_name().unwrap().to_string_lossy(),
            case["log_name"].as_str().unwrap()
        );
        for (path, key) in [(&saved.notes_path, "notes"), (&saved.log_path, "log")] {
            let expected = case[key].as_str().unwrap();
            let expected = if cfg!(windows) {
                expected.replace('\n', "\r\n")
            } else {
                expected.into()
            };
            assert_eq!(fs::read(path).unwrap(), expected.as_bytes());
        }
        assert_eq!(
            read_transcript_log(&saved.log_path).unwrap(),
            case["transcript"]
        );
        assert_eq!(
            latest_transcript_log(dir.path()).unwrap(),
            Some(saved.log_path)
        );
    }
}
#[test]
fn templates_overlay_invalid_files_and_unterminated_frontmatter() {
    let t=parse_template("team","---\r\nname: Team Sync\r\ndescription: The team\r\n---\r\nBe specific.\r\n## Updates\r\nOne per person.\r\n\r\n## Decisions\r\nNone if none.");
    assert_eq!(t.name, "Team Sync");
    assert_eq!(t.instructions, "Be specific.");
    assert_eq!(t.section_names(), ["Updates", "Decisions"]);
    assert_eq!(t.sections[0].guidance, "One per person.");
    let t = parse_template("x", "---\nname: Nope\n## Yes\nbody");
    assert_eq!(t.name, "x");
    assert!(t.instructions.contains("name: Nope"));
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("general.md"),
        "---\nname: Team\n---\n## Custom\nPlease",
    )
    .unwrap();
    fs::write(dir.path().join("bad.md"), "no sections").unwrap();
    fs::write(dir.path().join("huge.md"), "x".repeat(65537)).unwrap();
    fs::write(dir.path().join("bytes.md"), [0xff]).unwrap();
    let loaded = load_templates(Some(dir.path()));
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded["general"].name, "Team");
    assert_eq!(
        resolve_template(Some("unknown"), Some(dir.path())).name,
        "Team"
    );
    assert_eq!(resolve_template(Some("standup"), None).name, "Standup");
}
#[test]
fn headings_unicode_order_and_duplicates() {
    for (output, notes, expected) in [
        (
            "# pricing roadmap\n## customers status",
            "## **Pricing & Roadmap!**\n### Customer's status?",
            vec![],
        ),
        ("## Second\n## First", "## First\n## Second", vec!["Second"]),
        ("# 次のステップ", "# 価格", vec!["価格"]),
        ("## цены", "# Цены", vec![]),
        ("# ???", "# !!!", vec!["!!!"]),
        ("# !!!", "# !!!", vec![]),
        ("# launch date", "## Launch_Date", vec![]),
        ("# A", "# A\n# A", vec!["A"]),
    ] {
        assert_eq!(missing_user_headings(output, notes), expected);
    }
    assert_eq!(user_headings("## A\r\n### B\n"), ["A", "B"]);
}
#[test]
fn segments_stamps_speakers_and_empty_writer() {
    let mut segments = vec![Segment::new(0., "hello"), Segment::new(61.9, "world")];
    assert_eq!(segments[1].stamp(), "01:01");
    assert_eq!(format_transcript(&segments), "hello world");
    assert!(!has_speakers(&segments));
    segments[1].source = "others".into();
    assert_eq!(
        format_transcript(&segments),
        "[00:00] Me: hello\n[01:01] Others: world"
    );
    segments[1].source = "Dana".into();
    assert_eq!(segments[1].speaker(), "Dana");
    assert_eq!(Segment::new(-1., "").stamp(), "-1:59");
    let dir = tempfile::tempdir().unwrap();
    let writer = ObsidianWriter::new(dir.path(), Path::new("notes"), Path::new("logs"));
    let saved = writer
        .write(&WriteNote {
            title: "Meeting",
            category: "",
            started_at: when(),
            duration_seconds: 0.,
            notes_md: "",
            segments: &[],
            user_notes: "",
            template: "",
            generated_title: false,
        })
        .unwrap();
    assert!(fs::read_to_string(saved.log_path)
        .unwrap()
        .contains("(no speech captured)"));
    assert_eq!(
        latest_transcript_log(&dir.path().join("absent")).unwrap(),
        None
    );
    assert!(read_transcript_log(&dir.path().join("absent")).is_err());
}
#[test]
fn generated_titles_timeout_errors_empty_and_snapshot() {
    for title in [None, Some(""), Some("   meeting  "), Some("MEETING")] {
        assert!(is_default_title(title));
    }
    for title in ["Launch sync", "Meetings", "Zoom meeting"] {
        assert!(!is_default_title(Some(title)));
    }
    assert_eq!(fallback_title(when()), "Meeting Nov 14, 10:00 AM");
    for (raw, expected) in [
        ("2026-01-02 00:05:00", "Meeting Jan 2, 12:05 AM"),
        ("2026-07-04 13:00:00", "Meeting Jul 4, 1:00 PM"),
        ("2026-12-31 12:59:00", "Meeting Dec 31, 12:59 PM"),
    ] {
        assert_eq!(
            fallback_title(NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").unwrap()),
            expected
        );
    }
    let llm = Arc::new(Fake::new(&json!(["Title: Budget review"])));
    assert_eq!(
        generate_title(
            Some(llm.clone()),
            "Discuss >>> budget",
            "notes",
            when(),
            Duration::from_secs(1)
        ),
        ("Budget review".into(), true)
    );
    let calls = llm.calls.lock().unwrap();
    assert_eq!(calls[0][0], prompts::TITLE_SYSTEM);
    assert_eq!(calls[0][2], 24);
    drop(calls);
    for replies in [json!([null]), json!(["123"])] {
        assert_eq!(
            generate_title(
                Some(Arc::new(Fake::new(&replies))),
                "words",
                "",
                when(),
                Duration::from_secs(1)
            ),
            (fallback_title(when()), false)
        );
    }
    let llm = Arc::new(Fake {
        delay: Duration::from_secs(2),
        ..Fake::default()
    });
    let start = Instant::now();
    assert!(!generate_title(Some(llm), "words", "", when(), Duration::from_millis(5)).1);
    assert!(start.elapsed() < Duration::from_secs(1));
    let llm = Arc::new(Fake::default());
    assert!(!generate_title(Some(llm.clone()), " ", "", when(), Duration::from_secs(1)).1);
    assert!(llm.calls.lock().unwrap().is_empty());
    assert!(!generate_title(None, "words", "", when(), Duration::from_secs(1)).1);
    let long = title_prompt(&"t".repeat(10000), &"n".repeat(3000));
    let data = long
        .split("<<<\n")
        .nth(1)
        .unwrap()
        .split("\n>>>")
        .next()
        .unwrap();
    let data: Value = serde_json::from_str(data).unwrap();
    assert_eq!(data["notes"].as_str().unwrap().len(), 2000);
    assert_eq!(data["transcript"].as_str().unwrap().len(), 6000);
    let llm = OllamaLlm::new("http://localhost:11434", "gemma4", 300, 8192);
    let copy = title_client(Some(&llm), Duration::from_secs(20)).unwrap();
    assert_eq!(copy.timeout, 20);
    assert_eq!(llm.timeout, 300);
    assert_eq!(copy.model, llm.model);
    let cloud = OpenAiCompatLlm::new("https://example/v1", "m", "test-placeholder", 300, None);
    let copy = title_client(Some(&cloud), Duration::from_secs(20)).unwrap();
    assert!(copy.has_api_key());
    assert_eq!(copy.timeout, 20);
    assert_eq!(cloud.timeout, 300);
    assert!(title_client::<OllamaLlm>(None, Duration::from_secs(20)).is_none());
}
#[test]
fn writer_collisions_orphans_exhaustion_and_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let writer = ObsidianWriter::new(dir.path(), Path::new("notes"), Path::new("logs"));
    let note = WriteNote {
        title: "Sync",
        category: "Zoom",
        started_at: when(),
        duration_seconds: 60.,
        notes_md: "first",
        segments: &[],
        user_notes: "",
        template: "",
        generated_title: false,
    };
    let first = writer.write(&note).unwrap();
    let second = writer.write(&note).unwrap();
    assert!(second.notes_path.ends_with("2026-11-14 1000 Sync 2.md"));
    assert!(fs::read_to_string(&first.notes_path)
        .unwrap()
        .contains("first"));
    let orphan = second
        .log_path
        .parent()
        .unwrap()
        .join("2026-11-14 1000 Sync 3 (transcript).md");
    fs::write(&orphan, "orphan").unwrap();
    let fourth = writer.write(&note).unwrap();
    assert!(fourth.notes_path.ends_with("2026-11-14 1000 Sync 4.md"));
    assert!(!first
        .notes_path
        .parent()
        .unwrap()
        .join("2026-11-14 1000 Sync 3.md")
        .exists());
    assert_eq!(fs::read_to_string(orphan).unwrap(), "orphan");
    let pool = tempfile::tempdir().unwrap();
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let path = pool.path().to_owned();
            std::thread::spawn(move || writer::reserve(&path, &path, "Same").unwrap())
        })
        .collect();
    let mut names: Vec<_> = handles.into_iter().map(|h| h.join().unwrap().0).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 10);
    for i in 1..=MAX_NAME_ATTEMPTS {
        let name = if i == 1 {
            "Full".into()
        } else {
            format!("Full {i}")
        };
        fs::write(pool.path().join(format!("{name}.md")), "").unwrap();
    }
    assert_eq!(
        writer::reserve(pool.path(), pool.path(), "Full")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
}
