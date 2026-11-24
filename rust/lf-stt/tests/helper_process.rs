use lf_core::config::Config;
use lf_runtime::engine::SpeechEngine;
use lf_stt::helper::{serve_stdio, serve_stdio_with, HelperSpeechEngine};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fake(
    Config,
    std::sync::Mutex<Option<lf_stt::stt_lease::LeaseGuard>>,
);
impl SpeechEngine for Fake {
    fn load(&self) -> Result<(), String> {
        *self.1.lock().unwrap() = lf_stt::stt_lease::record("whisper-cpp", "tiny", None, true);
        #[cfg(unix)]
        println!("native stdout must not corrupt IPC");
        if self.0.field_str("model_size") == Some("load-error") {
            Err("fake model load failed".into())
        } else {
            Ok(())
        }
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        if let Some(path) = self.0.field_str("language") {
            if Path::new(path).exists() {
                std::fs::remove_file(path).unwrap();
                eprintln!("{} crash diagnostic tail", "x".repeat(20000));
                std::process::exit(17);
            }
        }
        if self.0.field_str("model_size") == Some("blocked") {
            std::thread::sleep(Duration::from_secs(120));
        }
        if pcm.is_empty() {
            return Err("empty fake input".into());
        }
        if self.0.field_bool("cache_only_marker") == Some(true) {
            return Ok(format!("cache-only:{}", pcm.len()));
        }
        Ok(format!(
            "{}:{}:{}",
            pcm.len(),
            pcm[0],
            prompt.unwrap_or_default()
        ))
    }
    fn unload(&self) {}
}

fn config(model: &str) -> Config {
    let mut config = Config::default();
    config.set_field("model_size", serde_json::json!(model));
    config
}

#[cfg(unix)]
fn gone(pid: u32) -> bool {
    // SAFETY: signal zero only queries whether a process exists.
    unsafe {
        libc::kill(pid as i32, 0) != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}
#[cfg(windows)]
fn gone(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: querying this test's known child id; successful handles are closed.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return true;
    }
    let mut code = 0;
    // SAFETY: handle was returned by OpenProcess and is closed after querying.
    unsafe {
        let running = GetExitCodeProcess(handle, &mut code) == 0 || code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        !running
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--speech-helper") {
        let legacy = std::env::args()
            .next()
            .is_some_and(|argv0| argv0.contains("legacy-helper"));
        if legacy {
            // A helper without cache-only support (plain `serve_stdio`).
            serve_stdio(|config| Ok(Arc::new(Fake(config, std::sync::Mutex::new(None))))).unwrap();
        } else {
            serve_stdio_with(|mut config, cache_only| {
                if cache_only {
                    config.set_field("cache_only_marker", serde_json::json!(true));
                }
                Ok(Arc::new(Fake(config, std::sync::Mutex::new(None))))
            })
            .unwrap();
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--parent") {
        let engine = HelperSpeechEngine::new(std::env::current_exe().unwrap(), &config("blocked"));
        engine.load().unwrap();
        let path = std::path::PathBuf::from(std::env::args().nth(2).unwrap());
        let pending = path.with_extension("pending");
        std::fs::write(&pending, engine.child_pid().unwrap().to_string()).unwrap();
        std::fs::rename(pending, path).unwrap();
        let _ = engine.transcribe(&[0.5], None);
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this standalone test sets its isolated home before starting threads.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("USERPROFILE", home.path());
    }
    let engine = Arc::new(HelperSpeechEngine::new(exe.clone(), &config("normal")));
    assert!(!engine.loaded());
    engine.load().unwrap();
    let pid = engine.child_pid().unwrap();
    assert!(engine.loaded());
    let jobs: Vec<_> = (0..4)
        .map(|i| {
            let engine = engine.clone();
            std::thread::spawn(move || {
                assert_eq!(
                    engine
                        .transcribe(&[0.25; 2049], Some(&i.to_string()))
                        .unwrap(),
                    format!("2049:0.25:{i}")
                )
            })
        })
        .collect();
    for job in jobs {
        job.join().unwrap();
    }
    engine.unload();
    assert!(gone(pid), "unload left child {pid}");
    no_leases(pid);
    assert!(!engine.loaded());
    engine.unload();
    engine.transcribe(&[0.5], None).unwrap();
    let pid = engine.child_pid().unwrap();
    drop(engine);
    assert!(gone(pid), "drop left child {pid}");
    no_leases(pid);

    let bad = HelperSpeechEngine::new(exe.clone(), &config("load-error"));
    assert!(bad.load().unwrap_err().contains("fake model load failed"));
    assert!(bad.child_pid().is_none());
    let missing =
        HelperSpeechEngine::new(exe.with_file_name("missing-executable"), &config("normal"));
    assert!(missing.load().unwrap_err().contains("spawn speech helper"));

    // Cache-only loads reach a helper that supports them, and only as such.
    let cached = HelperSpeechEngine::cached(exe.clone(), &config("normal"));
    assert_eq!(cached.transcribe(&[0.5; 3], None).unwrap(), "cache-only:3");
    let pid = cached.child_pid().unwrap();
    drop(cached);
    assert!(gone(pid));
    no_leases(pid);
    // A helper without cache-only support refuses before `make`/load runs.
    let copies = tempfile::tempdir().unwrap();
    let legacy_exe = copies
        .path()
        .join(format!("legacy-helper{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&exe, &legacy_exe).unwrap();
    let legacy = HelperSpeechEngine::cached(legacy_exe.clone(), &config("normal"));
    let error = legacy.load().unwrap_err();
    assert!(
        error.contains(lf_stt::helper::CACHE_ONLY_UNSUPPORTED),
        "{error}"
    );
    assert!(legacy.child_pid().is_none() && !legacy.loaded());
    // The ordinary load path on that helper is unchanged.
    let plain = HelperSpeechEngine::new(legacy_exe, &config("normal"));
    assert_eq!(plain.transcribe(&[0.5], None).unwrap(), "1:0.5:");
    drop(plain);

    let root = tempfile::tempdir().unwrap();
    let flag = root.path().join("crash-once");
    std::fs::write(&flag, "crash").unwrap();
    let mut crash = config("normal");
    crash.set_field("language", serde_json::json!(flag.to_str().unwrap()));
    let engine = HelperSpeechEngine::new(exe.clone(), &crash);
    engine.load().unwrap();
    let pid = engine.child_pid().unwrap();
    let error = engine.transcribe(&[1.0], None).unwrap_err();
    assert!(error.contains("crash diagnostic tail"), "{error}");
    assert!(error.len() < 9000);
    assert!(gone(pid));
    no_leases(pid);
    assert_eq!(
        engine.transcribe(&[1.0], Some("retry")).unwrap(),
        "1:1:retry"
    );
    let pid = engine.child_pid().unwrap();
    assert!(engine
        .transcribe(&[], None)
        .unwrap_err()
        .contains("empty fake input"));
    assert!(gone(pid));

    let blocked = Arc::new(HelperSpeechEngine::new(exe.clone(), &config("blocked")));
    blocked.load().unwrap();
    let pid = blocked.child_pid().unwrap();
    let job = {
        let blocked = blocked.clone();
        std::thread::spawn(move || blocked.transcribe(&[0.5], None))
    };
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    blocked.unload();
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(gone(pid));
    assert!(job.join().unwrap().is_err());
    no_leases(pid);

    let pid_file = root.path().join("orphan-pid");
    #[cfg(target_os = "linux")]
    // SAFETY: the test runner adopts/reaps its orphaned grandchild rather than PID 1.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let mut parent = std::process::Command::new(exe)
        .arg("--parent")
        .arg(&pid_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pid_file.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    parent.kill().unwrap();
    parent.wait().unwrap();
    while !gone(pid) {
        #[cfg(target_os = "linux")]
        // SAFETY: this is the adopted helper's pid; WNOHANG never blocks.
        unsafe {
            libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
        }
        assert!(Instant::now() < deadline, "orphan helper {pid}");
        std::thread::sleep(Duration::from_millis(10));
    }
    no_leases(pid);
    println!("Speech helper process tests: ready/load error, serialized PCM, crash diagnostics/respawn, unload/drop, busy shutdown and parent EOF passed");
}

fn no_leases(pid: u32) {
    let folder = lf_stt::stt_lease::lease_dir();
    for entry in std::fs::read_dir(folder).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            name != format!("{pid}.json") && !name.starts_with(&format!("{pid}.")),
            "helper left lease {name}"
        );
    }
}
