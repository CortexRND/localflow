//! The `meeting_sim` example's scripted-transcript seam, end to end against a
//! local mock Ollama (no network, no model).
#[path = "../examples/meeting_sim/sim.rs"]
mod sim;

use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Answers `POST /api/generate` like Ollama: notes for summaries, a citation
/// for chat questions. Records every request body.
struct MockOllama {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl MockOllama {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let stop: Arc<AtomicBool> = Arc::default();
        let (seen, done) = (bodies.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &seen),
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            url,
            bodies,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for MockOllama {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(mut stream: TcpStream, seen: &Mutex<Vec<Value>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        let mut byte = [0];
        if stream.read_exact(&mut byte).is_err() {
            return;
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
    };
    let headers = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    bytes.resize(end + length, 0);
    stream.read_exact(&mut bytes[end..]).unwrap();
    let body: Value = serde_json::from_slice(&bytes[end..]).unwrap_or_default();
    let prompt = body["prompt"].as_str().unwrap_or("").to_string();
    seen.lock().unwrap().push(body);
    let answer = if prompt.contains("Question:") {
        "We settled on twelve dollars per seat, and Marcus owns the page [1] [00:10]."
    } else {
        "# Pricing\n- Pro plan is twelve dollars per seat; fourteen day trial.\n\n# Next steps\n- legal review\n\n## Summary\nMarcus updates the pricing page by Friday the 14th; churn doubled above twelve; annual discounts on March 3.\n"
    };
    let response = json!({"response": answer, "done": true}).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn args(out: &Path, url: &str, scenario: &str, extra: &[&str]) -> sim::Args {
    let mut argv: Vec<String> = [
        "--scenario",
        scenario,
        "--out",
        out.to_str().unwrap(),
        "--scenarios",
        repo_root()
            .join("evals/meeting_sim/scenarios.json")
            .to_str()
            .unwrap(),
        "--llm-url",
        url,
        "--llm-model",
        "mock:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.extend(extra.iter().map(|s| s.to_string()));
    sim::parse_args(argv).unwrap()
}

fn scenario(id: &str) -> Value {
    sim::load_scenario(&repo_root().join("evals/meeting_sim/scenarios.json"), id).unwrap()
}

#[test]
fn parse_args_matches_driver_flags() {
    let a = sim::parse_args(
        [
            "--scenario=x",
            "--output",
            "o.json",
            "--llm-url",
            "http://h:1",
            "--llm-model",
            "m",
            "--sut",
            "/s",
            "--scenarios",
            "s.json",
            "--cleanup",
        ]
        .map(String::from),
    )
    .unwrap();
    assert_eq!(
        (a.scenario.as_str(), a.out.to_str(), a.cleanup),
        ("x", Some("o.json"), true)
    );
    assert_eq!(
        (a.llm_url.as_str(), a.llm_model.as_str()),
        ("http://h:1", "m")
    );
    assert_eq!(
        (a.sut.to_str(), a.scenarios.to_str()),
        (Some("/s"), Some("s.json"))
    );
    assert_eq!(a.llm_provider, "ollama");
    for bad in [
        vec!["--out", "o"],
        vec!["--scenario", "x"],
        vec!["--scenario", "x", "--out", "o", "--bogus", "1"],
        vec!["--scenario", "x", "--out"],
        vec!["--scenario", "x", "--out", "o", "--llm-provider", "nope"],
    ] {
        assert!(
            sim::parse_args(bad.iter().map(|s| s.to_string())).is_err(),
            "{bad:?}"
        );
    }
    assert!(sim::load_scenario(
        &repo_root().join("evals/meeting_sim/scenarios.json"),
        "nope"
    )
    .is_err());
}

#[test]
fn scripted_meeting_drives_the_backend_end_to_end() {
    let llm = MockOllama::start();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("r1/results/pricing-review.json");
    let summary = sim::run(&args(&out, &llm.url, "pricing-review", &["--cleanup"])).unwrap();
    assert_eq!(summary["stop"], 200, "{summary}");
    let res: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let sc = scenario("pricing-review");
    assert_eq!(res["scenario"], "pricing-review");
    assert_eq!(res["runtime"], "rust");
    assert_eq!(res["llm"], json!({"url": llm.url, "model": "mock:1"}));
    assert!(
        !Path::new(res["tmp"].as_str().unwrap()).exists(),
        "--cleanup"
    );

    // Every recorded HTTP call succeeded through the real router.
    let http = res["http"].as_array().unwrap();
    assert!(http.iter().all(|h| h["status"] == 200), "{http:?}");
    let calls: Vec<_> = http.iter().map(|h| h["call"].as_str().unwrap()).collect();
    assert_eq!(
        &calls[..2],
        ["GET /meeting/templates", "POST /meeting/start"]
    );
    assert!(calls.contains(&"POST /meeting/notes") && calls.contains(&"POST /meeting/stop"));
    assert_eq!(res["start"][1]["system_audio"], "on");

    // One transcript line per scripted line, in order, with speakers.
    let log = res["log_text"].as_str().unwrap().replace("\r\n", "\n");
    let mut at = 0;
    for (i, line) in sc["lines"].as_array().unwrap().iter().enumerate() {
        let who = if line[0] == "me" { "Me" } else { "Others" };
        let stamp = format!("**{:02}:{:02}**", i * 5 / 60, i * 5 % 60);
        let want = format!("{stamp} **{who}:** {}", line[1].as_str().unwrap());
        let found = log[at..]
            .find(&want)
            .unwrap_or_else(|| panic!("missing {want:?} in\n{log}"));
        at += found + want.len();
    }
    let notes_final = sc["notes_final"].as_str().unwrap().trim();
    assert!(
        log.contains(&format!("## My raw notes\n\n{notes_final}\n")),
        "{log}"
    );
    assert!(res["notes_text"]
        .as_str()
        .unwrap()
        .contains("twelve dollars"));
    assert_eq!(res["stop"][1]["template"], sc["template"]);
    assert_eq!(res["stop"][1]["enhanced"], true);

    // Chat answers per question, then a restart chat that finds the saved log.
    let chats = res["chats"].as_array().unwrap();
    assert_eq!(chats.len(), sc["chat"].as_array().unwrap().len());
    assert!(chats
        .iter()
        .all(|c| c["status"] == 200 && c["answer"].as_str().unwrap().contains("twelve")));
    assert_eq!(res["restart"]["status"], 200);
    assert_eq!(res["restart"]["log_path"], res["restart"]["expected"]);
    assert!(res["restart"]["log_path"].is_string());

    // Timings: the fields evals/meeting_sim/gates.py pools.
    let t = &res["timings"];
    let stop_ms = t["stop_to_notes_ms"].as_f64().unwrap();
    assert!(stop_ms > 0.0 && t["summarize_ms"].as_f64() == Some(stop_ms));
    assert_eq!(t["chat_ms"].as_array().unwrap().len(), chats.len());
    assert!(t["restart_chat_ms"].as_f64().unwrap() > 0.0);
    let llm_calls = res["llm_calls"].as_array().unwrap();
    // One summary, one call per chat question, one restart chat.
    assert_eq!(llm_calls.len(), 1 + chats.len() + 1);
    assert_eq!(t["llm_calls_count"], llm_calls.len());
    assert_eq!(t["llm_call_ms"].as_array().unwrap().len(), llm_calls.len());
    assert_eq!(res["summarize_calls"], 1);
    assert!(llm_calls
        .iter()
        .all(|c| c["ms"].is_f64() && c["secs"].is_f64() && c.get("error").is_none()));
    // The first (summary) prompt carries the scripted transcript and the user's notes.
    let first = llm_calls[0]["prompt"].as_str().unwrap();
    assert!(
        first.contains("Marcus will update the pricing page") && first.contains("legal review")
    );
    // The real Ollama provider made the calls (no shortcut around HTTP).
    let bodies = llm.bodies.lock().unwrap();
    assert_eq!(bodies.len(), llm_calls.len());
    assert!(bodies.iter().all(|b| b["model"] == "mock:1"));
}

#[test]
fn failed_system_audio_falls_back_to_mic_only() {
    let llm = MockOllama::start();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("vendor.json");
    sim::run(&args(&out, &llm.url, "vendor-call-micfallback", &[])).unwrap();
    let res: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let tmp = PathBuf::from(res["tmp"].as_str().unwrap());
    assert!(tmp.is_dir(), "kept without --cleanup");
    std::fs::remove_dir_all(&tmp).unwrap();
    assert_eq!(res["start"][1]["system_audio"], "error");
    assert_eq!(
        res["start"][1]["system_audio_error"],
        "sim: no loopback device"
    );
    assert!(res["status_mid"][1].to_string().contains("\"error\""));
    let log = res["log_text"].as_str().unwrap();
    assert!(!log.contains("**Others:**"), "{log}");
    let sc = scenario("vendor-call-micfallback");
    for line in sc["lines"].as_array().unwrap() {
        if line[0] == "me" {
            assert!(log.contains(line[1].as_str().unwrap()), "{log}");
        }
    }
}

#[test]
fn unreachable_llm_still_writes_a_scorable_result() {
    // A closed port: summary falls back, chat fails with 502 (recorded, never a crash).
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("x.json");
    sim::run(&args(
        &out,
        &format!("http://127.0.0.1:{port}"),
        "design-standup-nonotes",
        &["--cleanup"],
    ))
    .unwrap();
    let res: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert!(res["chats"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["status"] == 502));
    let calls = res["llm_calls"].as_array().unwrap();
    assert!(!calls.is_empty() && calls.iter().all(|c| c["error"].is_string()));
}

#[test]
fn diarized_meeting_emits_numbered_speaker_labels() {
    let llm = MockOllama::start();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("r1/results/launch-sync-diarized.json");
    let summary = sim::run(&args(
        &out,
        &llm.url,
        "launch-sync-diarized",
        &["--cleanup"],
    ))
    .unwrap();
    assert_eq!(summary["stop"], 200, "{summary}");
    let res: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(res["stop"][1]["diarization"], "ok", "{res}");
    let mut labels: Vec<&str> = res["stop"][1]["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["label"].as_str())
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["Speaker 1", "Speaker 2", "Speaker 3"]);
    let log = res["log_text"].as_str().unwrap().replace("\r\n", "\n");
    for label in &labels {
        assert!(log.contains(&format!("**{label}:**")), "{log}");
    }
    assert!(!log.contains("**Others:**"), "{log}");
}
