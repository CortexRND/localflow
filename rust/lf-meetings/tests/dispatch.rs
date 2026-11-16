use chrono::NaiveDateTime;
use lf_meetings::dispatch::*;
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier, Mutex},
    time::Duration,
};
fn fixture() -> Value {
    serde_json::from_str(include_str!("python-reference.json")).unwrap()
}
fn when() -> NaiveDateTime {
    NaiveDateTime::parse_from_str("2026-11-14 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
}
fn fields(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}
#[test]
fn timestamp_prompt_files_match_datetime_and_reject_invalid_timestamps() {
    use chrono::TimeZone;
    let dir = tempfile::tempdir().unwrap();
    let prompts = vec![WorkPrompt {
        title: "Task".into(),
        body: "Do it".into(),
    }];
    let dt = when();
    let ts = chrono::Local
        .from_local_datetime(&dt)
        .single()
        .unwrap()
        .timestamp() as f64;
    let a = write_prompts(&prompts, "Sync", dt, &dir.path().join("a")).unwrap();
    let b = write_prompts(&prompts, "Sync", ts, &dir.path().join("b")).unwrap();
    assert_eq!(a[0].file_name(), b[0].file_name());
    assert_eq!(fs::read(&a[0]).unwrap(), fs::read(&b[0]).unwrap());
    for ts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
        assert!(write_prompts(&prompts, "Sync", ts, dir.path()).is_err());
    }
    assert!(
        write_prompts(&[], "Sync", f64::NAN, &dir.path().join("empty"))
            .unwrap()
            .is_empty()
    );
    assert!(!dir.path().join("empty").exists());
}
#[test]
fn frozen_parse_slugs_headers_prompt_files_and_queue_bytes() {
    let f = fixture();
    for case in f["parse"].as_array().unwrap() {
        assert_eq!(
            json!(parse_work_prompts(case["input"].as_str().unwrap())),
            case["output"]
        );
    }
    for case in f["slugs"].as_array().unwrap() {
        assert_eq!(slugify(case["input"].as_str().unwrap()), case["output"]);
    }
    for case in f["headers"].as_array().unwrap() {
        assert_eq!(
            header_value(case["input"].as_str().unwrap()),
            case["output"]
        );
    }
    let prompts = parse_work_prompts(f["parse"][0]["input"].as_str().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let paths = write_prompts(&prompts, "Weekly Sync", when(), dir.path()).unwrap();
    for (path, expected) in paths.iter().zip(f["prompt_files"].as_array().unwrap()) {
        let rel = path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(rel, expected["name"]);
        let s = expected["body"].as_str().unwrap();
        let s = if cfg!(windows) {
            s.replace('\n', "\r\n")
        } else {
            s.into()
        };
        assert_eq!(fs::read(path).unwrap(), s.as_bytes());
    }
    let queue = PromptQueue::new(dir.path().join("queue.json"));
    let entries = queue
        .add_at(
            &prompts,
            &[
                PathBuf::from("/fixtures/one.md"),
                PathBuf::from("/fixtures/two.md"),
            ],
            "Weekly Sync",
            when(),
        )
        .unwrap();
    assert_eq!(
        json!(entries),
        f[if cfg!(windows) {
            "queue_entries_windows"
        } else {
            "queue_entries"
        }]
    );
    let expected = f[if cfg!(windows) {
        "queue_bytes_windows"
    } else {
        "queue_bytes"
    }]
    .as_str()
    .unwrap();
    let expected = if cfg!(windows) {
        expected.replace('\n', "\r\n")
    } else {
        expected.into()
    };
    assert_eq!(fs::read(&queue.path).unwrap(), expected.as_bytes());
    assert!(!queue
        .path
        .with_extension(format!("json.{}.tmp", std::process::id()))
        .exists());
    assert_eq!(
        write_prompts(&prompts, "Weekly Sync", when(), dir.path()).unwrap(),
        paths
    );
    assert!(write_prompts(&[], "X", when(), &dir.path().join("nothing"))
        .unwrap()
        .is_empty());
    assert!(!dir.path().join("nothing").exists());
}
fn seed(dir: &Path) -> (PromptQueue, Vec<Value>) {
    let queue = PromptQueue::new(dir.join("state/queue.json"));
    let prompts = vec![
        WorkPrompt {
            title: "One".into(),
            body: "body".into(),
        },
        WorkPrompt {
            title: "Two".into(),
            body: "body2".into(),
        },
    ];
    let entries = queue
        .add_at(
            &prompts,
            &[dir.join("one.md"), dir.join("two.md")],
            "Weekly",
            when(),
        )
        .unwrap();
    (queue, entries)
}
#[test]
fn queue_empty_corrupt_reload_status_filter_and_fields() {
    let dir = tempfile::tempdir().unwrap();
    let q = PromptQueue::new(dir.path().join("empty.json"));
    assert!(q.list(None).unwrap().is_empty());
    assert!(q.get("none").unwrap().is_none());
    for data in ["broken", "{}", "null", "42"] {
        fs::write(&q.path, data).unwrap();
        assert!(q.list(None).unwrap().is_empty());
    }
    let (q, entries) = seed(dir.path());
    let id = entries[0]["id"].as_str().unwrap();
    assert_eq!(q.list(Some("pending")).unwrap().len(), 2);
    assert!(q.list(Some("dispatched")).unwrap().is_empty());
    for status in STATUSES {
        let updated = q
            .set_status(id, status, None, &Map::new())
            .unwrap()
            .unwrap();
        assert_eq!(updated["status"], *status);
    }
    q.set_status(
        id,
        "failed",
        None,
        &fields(json!({"error":"orca failed","worktree":"wt","handle":"h","repo":"path:/x"})),
    )
    .unwrap();
    let approved = q
        .set_status(id, "approved", None, &fields(json!({"handle":"override"})))
        .unwrap()
        .unwrap();
    assert_eq!(approved["error"], "");
    assert_eq!(approved["worktree"], "");
    assert_eq!(approved["handle"], "override");
    assert_eq!(approved["repo"], "path:/x");
    let second = PromptQueue::new(q.path.clone());
    assert_eq!(second.get(id).unwrap().unwrap(), approved);
    assert!(q
        .set_status("missing", "failed", None, &Map::new())
        .unwrap()
        .is_none());
    assert_eq!(
        q.set_status("missing", "bogus", None, &Map::new())
            .unwrap_err()
            .to_string(),
        "unknown status 'bogus'; expected one of approved, dispatched, failed, pending, rejected"
    );
    assert!(q.path.with_extension("json.lock").exists() || !cfg!(unix));
}
#[test]
fn queue_idempotent_zip_and_deduplication_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let (q, entries) = seed(dir.path());
    let id = entries[0]["id"].as_str().unwrap();
    let updated = q
        .set_status(
            id,
            "dispatched",
            None,
            &fields(json!({"worktree":"wt","handle":"h"})),
        )
        .unwrap()
        .unwrap();
    let prompts = vec![
        WorkPrompt {
            title: "One".into(),
            body: "changed".into(),
        },
        WorkPrompt {
            title: "One".into(),
            body: "again".into(),
        },
        WorkPrompt {
            title: "Other".into(),
            body: "extra".into(),
        },
    ];
    let paths = vec![dir.path().join("one.md"), dir.path().join("one.md")];
    let repeated = q.add_at(&prompts, &paths, "Different", when()).unwrap();
    assert_eq!(repeated, vec![updated.clone(), updated]);
    assert_eq!(q.list(None).unwrap().len(), 2);
    assert_ne!(entry_id(&paths[0], "One"), entry_id(&paths[0], "Other"));
    assert_ne!(
        entry_id(&paths[0], "One"),
        entry_id(Path::new("different"), "One")
    );
}
#[test]
fn compare_and_set_transitions_retry_and_final_states() {
    let dir = tempfile::tempdir().unwrap();
    let (q, entries) = seed(dir.path());
    let id = entries[0]["id"].as_str().unwrap();
    assert_eq!(approve_prompt(&q, id).unwrap()["status"], "approved");
    let error = reject_prompt(&q, id).unwrap_err();
    let error = error.downcast_ref::<PromptTransitionError>().unwrap();
    assert_eq!(error.status_code, 409);
    assert_eq!(
        error.detail,
        "cannot reject a approved prompt (allowed from: pending, failed)"
    );
    let error = q
        .set_status(id, "rejected", Some(&["pending"]), &Map::new())
        .unwrap_err();
    let conflict = error.downcast_ref::<StatusConflict>().unwrap();
    assert_eq!(conflict.current, "approved");
    assert_eq!(
        conflict.to_string(),
        format!("cannot move {id} from 'approved'; expected one of pending")
    );
    q.set_status(
        id,
        "failed",
        Some(&["approved"]),
        &fields(json!({"error":"failed","worktree":"old","handle":"old"})),
    )
    .unwrap();
    assert_eq!(approve_prompt(&q, id).unwrap()["error"], "");
    q.set_status(id, "failed", None, &Map::new()).unwrap();
    assert_eq!(reject_prompt(&q, id).unwrap()["status"], "rejected");
    let error = approve_prompt(&q, "gone").unwrap_err();
    let error = error.downcast_ref::<PromptTransitionError>().unwrap();
    assert_eq!(error.status_code, 404);
    assert_eq!(error.detail, "unknown prompt: gone");
    assert!(q
        .set_status("gone", "approved", Some(APPROVE_FROM), &Map::new())
        .unwrap()
        .is_none());
}
#[test]
fn queue_threads_preserve_every_write_and_only_one_cas_wins() {
    let dir = tempfile::tempdir().unwrap();
    let (q, entries) = seed(dir.path());
    let q = Arc::new(q);
    let id = entries[0]["id"].as_str().unwrap().to_string();
    let barrier = Arc::new(Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let q = q.clone();
            let id = id.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                q.set_status(
                    &id,
                    "pending",
                    None,
                    &fields(json!({format!("field{i}"):i})),
                )
                .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let entry = q.get(&id).unwrap().unwrap();
    for i in 0..12 {
        assert_eq!(entry[format!("field{i}")], i);
    }
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let q = q.clone();
            let id = id.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                q.set_status(&id, "dispatched", Some(&["pending"]), &Map::new())
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter(|h| h.thread().id() != std::thread::current().id())
            .map(|h| h.join().unwrap() as usize)
            .sum::<usize>(),
        1
    );
}
#[cfg(unix)]
#[test]
fn queue_cross_process_child() {
    let Some(dir) = std::env::var_os("LF_QUEUE_TEST_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let q = PromptQueue::new(dir.join("queue.json"));
    if let Ok(id) = std::env::var("LF_QUEUE_TEST_CAS") {
        let q = PromptQueue::new(dir.join("state/queue.json"));
        match q.set_status(&id, "approved", Some(&["pending"]), &Map::new()) {
            Ok(Some(_)) => {
                fs::write(dir.join(format!("winner-{}", std::process::id())), "").unwrap()
            }
            Err(e) if e.downcast_ref::<StatusConflict>().is_some() => (),
            other => panic!("unexpected CAS result: {other:?}"),
        }
        return;
    }
    let title = std::env::var("LF_QUEUE_TEST_TITLE").unwrap();
    for i in 0..15 {
        let name = format!("{title}-{i}");
        q.add(
            &[WorkPrompt {
                title: name.clone(),
                body: "body".into(),
            }],
            &[dir.join(format!("{name}.md"))],
            "Meeting",
        )
        .unwrap();
    }
}
#[cfg(unix)]
#[test]
fn queue_cross_process_lock_keeps_every_write() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut children: Vec<_> = (0..3)
        .map(|i| {
            std::process::Command::new(&exe)
                .args(["--exact", "queue_cross_process_child", "--nocapture"])
                .env("LF_QUEUE_TEST_DIR", dir.path())
                .env("LF_QUEUE_TEST_TITLE", format!("child{i}"))
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in &mut children {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(
        PromptQueue::new(dir.path().join("queue.json"))
            .list(None)
            .unwrap()
            .len(),
        45
    );
}
#[cfg(unix)]
#[test]
fn queue_cross_process_cas_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let (queue, entries) = seed(dir.path());
    let exe = std::env::current_exe().unwrap();
    let mut children: Vec<_> = (0..5)
        .map(|_| {
            std::process::Command::new(&exe)
                .args(["--exact", "queue_cross_process_child", "--nocapture"])
                .env("LF_QUEUE_TEST_DIR", dir.path())
                .env("LF_QUEUE_TEST_CAS", entries[0]["id"].as_str().unwrap())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(
        fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("winner-"))
            .count(),
        1
    );
    assert_eq!(
        queue
            .get(entries[0]["id"].as_str().unwrap())
            .unwrap()
            .unwrap()["status"],
        "approved"
    );
}
#[test]
fn unavailable_sidecar_lock_does_not_prevent_queue_writes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("state/queue.json.lock")).unwrap();
    let (queue, entries) = seed(dir.path());
    let id = entries[0]["id"].as_str().unwrap();
    queue.set_status(id, "approved", None, &Map::new()).unwrap();
    assert_eq!(queue.get(id).unwrap().unwrap()["status"], "approved");
}
struct Runner {
    reply: Mutex<Option<Result<CommandOutput, CommandError>>>,
    calls: Mutex<Vec<(Vec<String>, Duration)>>,
}
impl Runner {
    fn output(code: i32, out: &str, err: &str) -> Self {
        Self {
            reply: Mutex::new(Some(Ok(CommandOutput {
                returncode: code,
                stdout: out.into(),
                stderr: err.into(),
            }))),
            calls: Mutex::new(Vec::new()),
        }
    }
    fn error(e: CommandError) -> Self {
        Self {
            reply: Mutex::new(Some(Err(e))),
            calls: Mutex::new(Vec::new()),
        }
    }
}
impl CommandRunner for Runner {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<CommandOutput, CommandError> {
        self.calls.lock().unwrap().push((argv.into(), timeout));
        self.reply.lock().unwrap().take().unwrap()
    }
}
#[test]
fn orca_available_success_down_headless_and_error_cases() {
    let runner = Runner::output(
        0,
        r#"noise {"ok":true,"result":{"app":{"running":true},"runtime":{"reachable":true}}}"#,
        "benign SecCodeCheckValidity",
    );
    assert_eq!(orca_available_with(&runner, false), (true, "".into()));
    assert_eq!(
        runner.calls.lock().unwrap()[0],
        (
            vec!["orca".into(), "status".into(), "--json".into()],
            Duration::from_secs(15)
        )
    );
    for(headless,data,expected)in[(false,json!({}),"Orca app is not running — run `orca open`"),(true,json!({}),"Orca app is not running — run `orca open` (not possible over SSH: it launches a GUI app on the Mac's own display)"),(false,json!({"result":{"app":{"running":true},"runtime":{"state":"down"}}}),"Orca runtime not reachable (down) — run `orca open`"),(false,json!({"result":{"app":{"running":true}}}),"Orca runtime not reachable (unknown) — run `orca open`")]{assert_eq!(orca_available_with(&Runner::output(0,&data.to_string(),""),headless),(false,expected.into()));}
    for (error, expected) in [
        (CommandError::NotFound, "orca CLI not found on PATH"),
        (CommandError::Timeout, "orca status timed out"),
        (
            CommandError::Other("boom".into()),
            "orca status failed: boom",
        ),
    ] {
        assert_eq!(
            orca_available_with(&Runner::error(error), false),
            (false, expected.into())
        );
    }
    assert_eq!(
        orca_available_with(&Runner::output(2, "", ""), false),
        (false, "orca status exited 2".into())
    );
    assert_eq!(
        orca_available_with(&Runner::output(0, "noise", ""), false),
        (
            false,
            "unreadable orca status output: no JSON in output".into()
        )
    );
}
#[test]
fn dispatch_argv_handles_repo_passthrough_and_no_queue_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt.md");
    fs::write(&path, "---\ntitle: Demo\n---\n\nDo the work.\n").unwrap();
    let entry = json!({"title":"Migrate session storage to Redis","path":path});
    for result in [
        json!({"worktree":{"worktreeId":"repo::/wt"},"agentTerminalHandle":"term"}),
        json!({"id":"repo::/wt","startupTerminal":{"handle":"term"}}),
        json!({"path":"repo::/wt"}),
    ] {
        let data = json!({"ok":true,"result":result});
        let runner = Runner::output(0, &format!("noise\n{data}"), "benign warning");
        let reply = dispatch_with(&entry, "name:My Repository", "claude", &runner);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["worktree"], "repo::/wt");
        assert_eq!(reply["raw"], data);
        if result.get("path").is_none() {
            assert_eq!(reply["handle"], "term");
        } else {
            assert_eq!(reply["handle"], "");
        }
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0].1, Duration::from_secs(120));
        assert_eq!(
            calls[0].0,
            [
                "orca",
                "worktree",
                "create",
                "--repo",
                "name:My Repository",
                "--name",
                "migrate-session-storage-to-redis",
                "--agent",
                "claude",
                "--prompt",
                "Do the work.",
                "--no-parent",
                "--setup",
                "run",
                "--json"
            ]
        );
    }
    assert!(!dir.path().join("queue.json").exists());
    assert_eq!(
        worktree_id(&json!({"result":{"worktree":{}}}), "fallback"),
        "fallback"
    );
    assert_eq!(
        worktree_id(&json!({"result":{"branch":"branch"}}), "fallback"),
        "branch"
    );
    assert_eq!(
        terminal_handle(
            &json!({"result":{"agentTerminalHandle":42,"startupTerminal":{"handle":"old"}}})
        ),
        "old"
    );
}
#[test]
fn dispatch_failures_and_frontmatter_edges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt.md");
    let entry = json!({"path":path,"title":""});
    assert!(
        dispatch_with(&entry, "repo", "agent", &Runner::output(0, "{}", ""))
            .get("error")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("cannot read prompt file")
    );
    fs::write(&path, "---\ntitle: empty\n---\n\n").unwrap();
    assert_eq!(
        dispatch_with(&entry, "repo", "agent", &Runner::output(0, "{}", ""))["error"],
        format!("prompt file is empty: {}", path.display())
    );
    fs::write(&path, "body").unwrap();
    for (error, expected) in [
        (CommandError::NotFound, "orca CLI not found on PATH"),
        (
            CommandError::Timeout,
            "orca worktree create timed out after 120s",
        ),
        (
            CommandError::Other("bad".into()),
            "orca worktree create failed: bad",
        ),
    ] {
        assert_eq!(
            dispatch_with(&entry, "repo", "agent", &Runner::error(error))["error"],
            expected
        );
    }
    for (code, out, err, expected) in [
        (
            2,
            "",
            "warning\nrepo missing\n",
            "orca exited 2: repo missing",
        ),
        (0, "noise", "", "unreadable orca output: no JSON in output"),
        (
            0,
            r#"{"ok":false,"error":"denied"}"#,
            "",
            "orca reported failure: denied",
        ),
        (
            0,
            r#"{"ok":false}"#,
            "",
            "orca reported failure: {'ok': False}",
        ),
        (
            0,
            r#"{"ok":false,"error":{"code":42}}"#,
            "",
            "orca reported failure: {'code': 42}",
        ),
    ] {
        assert_eq!(
            dispatch_with(&entry, "repo", "agent", &Runner::output(code, out, err))["error"],
            expected
        );
    }
    assert_eq!(
        strip_frontmatter("---\na: x\n---oops\nbody"),
        "---\na: x\n---oops\nbody"
    );
    assert_eq!(strip_frontmatter("---\r\na: x\r\n---\r\n\nbody"), "body");
    assert_eq!(strip_frontmatter("no header"), "no header");
    assert_eq!(last_line(" \nline\n"), "line");
    assert_eq!(last_line(""), "no output");
    assert_eq!(last_line(&"字".repeat(250)).chars().count(), 200);
    assert!(default_prompts_dir()
        .unwrap()
        .ends_with("projs/prompts/meetings"));
    assert!(queue_path().unwrap().ends_with(".localflow/queue.json"));
}
#[test]
fn process_runner_real_mock_executable_and_timeout() {
    let exe = std::env::current_exe().unwrap();
    let argv = vec![
        exe.to_string_lossy().into(),
        "--exact".into(),
        "runner_child".into(),
        "--nocapture".into(),
    ];
    let result = ProcessRunner.run(&argv, Duration::from_secs(5)).unwrap();
    assert_eq!(result.returncode, 0);
    assert!(result.stdout.contains("fake executable output"));
    assert!(result.stderr.contains("fake stderr"));
    let result = ProcessRunner.run(
        &["localflow-definitely-missing-executable".into()],
        Duration::from_secs(1),
    );
    assert!(matches!(result, Err(CommandError::NotFound)));
    let argv = vec![
        exe.to_string_lossy().into(),
        "--exact".into(),
        "runner_slow_child".into(),
    ];
    assert!(matches!(
        ProcessRunner.run(&argv, Duration::from_millis(1)),
        Err(CommandError::Timeout)
    ));
}
#[test]
fn runner_child() {
    println!("fake executable output");
    eprintln!("fake stderr");
}
#[test]
fn runner_slow_child() {
    if std::env::args().any(|s| s == "--exact") {
        std::thread::sleep(Duration::from_secs(2));
    }
}
