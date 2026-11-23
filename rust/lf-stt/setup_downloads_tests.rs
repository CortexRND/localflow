use super::*;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::sync::{atomic::Ordering, mpsc, Arc, Mutex};
use std::time::Duration;

enum Reply {
    Body(u16, String),
    Stall(mpsc::Sender<bool>),
}
fn server(replies: Vec<Reply>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = std::thread::spawn(move || {
        for reply in replies {
            let started = std::time::Instant::now();
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(5) {
                            return;
                        }
                        std::thread::park_timeout(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            match reply {
                Reply::Body(code, body) => {
                    let _=write!(reader.get_mut(),"HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                }
                Reply::Stall(closed) => {
                    reader
                        .get_mut()
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1001\r\n\r\n")
                        .unwrap();
                    reader.get_mut().write_all(&[0; 1000]).unwrap();
                    let _ = closed.send(matches!(reader.read(&mut [0]), Ok(0)));
                }
            }
        }
    });
    (url, thread)
}
fn model() -> NativeModel {
    NativeModel {
        repo: "test/native".into(),
        files: vec!["weights.bin".into()],
        format: "GGML",
    }
}
fn metadata(bytes: &[u8]) -> String {
    serde_json::json!({"sha":"a".repeat(40),"siblings":[{"rfilename":"weights.bin","size":bytes.len(),"lfs":{"sha256":format!("{:x}",Sha256::digest(bytes))}}]}).to_string()
}

#[test]
fn native_download_installs_standard_hf_snapshot_and_reports_exact_size() {
    let root = tempfile::tempdir().unwrap();
    let native = model();
    assert_eq!(native.inspect(root.path()), Inspection::default());
    let bytes = b"native weights";
    let (url, server) = server(vec![
        Reply::Body(200, metadata(bytes)),
        Reply::Body(200, String::from_utf8_lossy(bytes).into_owned()),
    ]);
    let events = Mutex::new(Vec::new());
    native
        .download(
            root.path(),
            &url,
            &|d, t| events.lock().unwrap().push((d, t)),
            &AtomicBool::new(false),
        )
        .unwrap();
    server.join().unwrap();
    assert_eq!(
        native.inspect(root.path()),
        Inspection {
            installed: true,
            bytes: bytes.len() as u64
        }
    );
    let file = hf_hub::Cache::new(root.path().into())
        .model(native.repo.clone())
        .get("weights.bin")
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    assert_eq!(
        *events.lock().unwrap().last().unwrap(),
        (bytes.len() as u64, bytes.len() as u64)
    );
    std::fs::remove_file(file).unwrap();
    assert!(!native.inspect(root.path()).installed);
    let (url, thread) = self::server(vec![Reply::Body(200, metadata(bytes))]);
    native
        .download(root.path(), &url, &|_, _| {}, &AtomicBool::new(false))
        .unwrap();
    thread.join().unwrap();
    assert!(native.inspect(root.path()).installed);
}

#[test]
fn missing_files_metadata_integrity_and_network_never_report_installed() {
    for (meta, code, body) in [
        (metadata(b"abc"), 500, "unavailable"),
        (metadata(b"abc"), 200, "bad"),
        (metadata(b"abc"), 200, "ab"),
        ("{}".into(), 200, ""),
        (
            serde_json::json!({"sha":"a".repeat(40),"siblings":[]}).to_string(),
            200,
            "",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let native = model();
        let mut replies = vec![Reply::Body(200, meta)];
        if !body.is_empty() {
            replies.push(Reply::Body(code, body.into()));
        }
        let (url, server) = server(replies);
        assert!(native
            .download(root.path(), &url, &|_, _| {}, &AtomicBool::new(false))
            .is_err());
        assert!(!native.inspect(root.path()).installed);
        server.join().unwrap();
    }
    let root = tempfile::tempdir().unwrap();
    assert!(model()
        .download(
            root.path(),
            "http://127.0.0.1:0",
            &|_, _| {},
            &AtomicBool::new(false)
        )
        .is_err());
    let mut invalid = model();
    invalid.files = vec!["../outside".into()];
    assert!(invalid
        .download(
            root.path(),
            "http://127.0.0.1:0",
            &|_, _| {},
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .contains("Invalid native"));
}

#[test]
fn cancel_aborts_stalled_download_and_removes_partial_not_cache() {
    let root = tempfile::tempdir().unwrap();
    let (ready, entered) = mpsc::channel();
    let (closed, disconnected) = mpsc::channel();
    let (url, server) = server(vec![
        Reply::Body(200, metadata(&vec![0; 1000])),
        Reply::Stall(closed),
    ]);
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let path = root.path().to_path_buf();
    let (reply, done) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        reply
            .send(model().download(
                &path,
                &url,
                &|done, _| {
                    if done == 1000 {
                        ready.send(()).unwrap();
                    }
                },
                &stop,
            ))
            .unwrap()
    });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let blobs = root.path().join("models--test--native/blobs");
    let partial = std::fs::read_dir(&blobs).unwrap().next().unwrap().unwrap();
    assert_eq!(std::fs::read(partial.path()).unwrap(), vec![0; 1000]);
    cancel.store(true, Ordering::Release);
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err()
        .contains("cancelled"));
    assert!(disconnected.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
    server.join().unwrap();
    assert!(!model().inspect(root.path()).installed);
    assert_eq!(
        std::fs::read_dir(root.path().join("models--test--native/blobs"))
            .unwrap()
            .count(),
        0
    );
    assert!(model()
        .download(
            root.path(),
            "http://127.0.0.1:0",
            &|_, _| panic!("cancelled"),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("cancelled"));
}

/// A future that resolves once `fire` is called from another thread.
#[derive(Clone, Default)]
struct Trigger(Arc<Mutex<(bool, Option<std::task::Waker>)>>);
impl Trigger {
    fn fire(&self) {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        if let Some(waker) = state.1.take() {
            waker.wake();
        }
    }
    async fn wait(self) {
        std::future::poll_fn(|cx| {
            let mut state = self.0.lock().unwrap();
            if state.0 {
                return std::task::Poll::Ready(());
            }
            state.1 = Some(cx.waker().clone());
            std::task::Poll::Pending
        })
        .await
    }
}

#[test]
fn snapshot_interrupt_drops_active_transfer_and_writes_no_ref() {
    let root = tempfile::tempdir().unwrap();
    let (ready, entered) = mpsc::channel();
    let (closed, disconnected) = mpsc::channel();
    let (url, server) = server(vec![
        Reply::Body(200, metadata(&vec![0; 1000])),
        Reply::Stall(closed),
    ]);
    let trigger = Trigger::default();
    let interrupt = trigger.clone().wait();
    let path = root.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        download_snapshot(
            "test/native",
            &path,
            &url,
            |done, _| {
                if done == 1000 {
                    ready.send(()).unwrap();
                }
            },
            interrupt,
        )
    });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let repo = root.path().join("models--test--native");
    let partial = std::fs::read_dir(repo.join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(partial.path()).unwrap(), vec![0; 1000]);
    trigger.fire();
    assert_eq!(worker.join().unwrap(), Err(INTERRUPTED.to_owned()));
    // The transfer future was dropped: connection closed, temp file gone.
    assert!(disconnected.recv_timeout(Duration::from_secs(2)).unwrap());
    server.join().unwrap();
    assert!(!partial.path().exists());
    assert_eq!(std::fs::read_dir(repo.join("blobs")).unwrap().count(), 0);
    assert!(!repo.join("refs").exists());
    assert_eq!(
        std::fs::read_dir(repo.join("snapshots").join("a".repeat(40)))
            .unwrap()
            .count(),
        0
    );
    // An interrupt that never fires leaves a normal snapshot download intact.
    let bytes = b"snapshot weights";
    let (url, server) = self::server(vec![
        Reply::Body(200, metadata(bytes)),
        Reply::Body(200, String::from_utf8_lossy(bytes).into_owned()),
    ]);
    download_snapshot(
        "test/native",
        root.path(),
        &url,
        |_, _| {},
        std::future::pending(),
    )
    .unwrap();
    server.join().unwrap();
    assert!(model().inspect(root.path()).installed);
}

#[test]
fn native_formats_are_separate_from_python_and_require_all_onnx_files() {
    #[cfg(feature = "whisper")]
    {
        let native = native_model("mlx-whisper", "base").unwrap();
        assert_eq!(native.repo, model_storage::WHISPER_CPP_REPO);
        assert_eq!(native.files, vec!["ggml-base.bin"]);
        assert_eq!(native.format, "GGML");
        let root = tempfile::tempdir().unwrap();
        let python_repo = model_storage::faster_whisper_repo("base").unwrap();
        let directory = root.path().join(format!(
            "models--{}/snapshots/{}",
            python_repo.replace('/', "--"),
            "b".repeat(40)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("model.bin"), b"python weights").unwrap();
        let python = hf_hub::Cache::new(root.path().into()).model(python_repo);
        python.create_ref(&"b".repeat(40)).unwrap();
        assert!(python.get("model.bin").is_some());
        assert!(!native.inspect(root.path()).installed);
        assert!(native_model("faster-whisper", "distil-large-v3").is_err());
    }
    #[cfg(feature = "parakeet")]
    {
        let root = tempfile::tempdir().unwrap();
        let native = native_model("parakeet", "large-v3").unwrap();
        let directory = root.path().join(format!(
            "models--{}/snapshots/{}",
            native.repo.replace('/', "--"),
            "a".repeat(40)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let cache = hf_hub::Cache::new(root.path().into()).model(native.repo.clone());
        cache.create_ref(&"a".repeat(40)).unwrap();
        for file in native.files.iter().take(3) {
            std::fs::write(directory.join(file), b"weights").unwrap();
        }
        assert!(!native.inspect(root.path()).installed);
        std::fs::write(directory.join(native.files.last().unwrap()), b"tokens").unwrap();
        assert!(native.inspect(root.path()).installed);
        assert_eq!(native.format, "ONNX");
    }
}
