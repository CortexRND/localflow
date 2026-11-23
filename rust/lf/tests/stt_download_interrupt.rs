//! `lf-rs stt download` interrupted by a real SIGINT mid-transfer: the
//! partial temp file is removed, no ref/snapshot is written, and the CLI
//! ends like click's KeyboardInterrupt handling ("Aborted!", exit 1).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SIZE: usize = 1000;

/// Serves metadata, then sends all but the last byte of the file and waits
/// for the client to drop the connection. Reports whether it saw EOF.
fn stalling_server() -> (String, mpsc::Receiver<bool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (closed, disconnected) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        // The digest is never checked: the transfer is interrupted first.
        let metadata = format!(
            r#"{{"sha":"{SHA}","siblings":[{{"rfilename":"model.bin","size":{SIZE},"lfs":{{"sha256":"{}"}}}}]}}"#,
            "f".repeat(64)
        );
        for stall in [false, true] {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let stream = reader.get_mut();
            if !stall {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{metadata}",
                    metadata.len()
                )
                .unwrap();
                continue;
            }
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\n\r\n").unwrap();
            stream.write_all(&[0; SIZE - 1]).unwrap();
            stream.flush().unwrap();
            let _ = closed.send(matches!(reader.read(&mut [0]), Ok(0)));
        }
    });
    (url, disconnected, thread)
}

fn files_in(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default()
}

#[test]
fn ctrl_c_during_stt_download_removes_partial_and_aborts() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let cache = sandbox.path().join("hub");
    std::fs::create_dir_all(&home).unwrap();
    let (url, disconnected, server) = stalling_server();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lf-rs"));
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if key.starts_with("HF_")
            || key.starts_with("HUGGINGFACE_")
            || key.starts_with("LOCALFLOW_")
            || key.starts_with("XDG_")
            || key == "TRANSFORMERS_OFFLINE"
        {
            command.env_remove(&key);
        }
    }
    let mut child = command
        .args(["stt", "download", "fw-base"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.path().join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.path().join("xdg-data"))
        .env("LOCALFLOW_CONFIG", sandbox.path().join("config.toml"))
        .env("HF_ENDPOINT", &url)
        .env("HF_HUB_CACHE", &cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait until the transfer is active: a temp file holds the partial body.
    let started = Instant::now();
    let (repo, partial) = loop {
        let partial = files_in(&cache)
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("models--")
            })
            .find_map(|repo| {
                let partial = files_in(&repo.join("blobs")).into_iter().next()?;
                let len = std::fs::metadata(&partial).ok()?.len();
                (len == (SIZE - 1) as u64).then_some((repo, partial))
            });
        if let Some(found) = partial {
            break found;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "transfer never became active"
        );
        assert!(child.try_wait().unwrap().is_none(), "lf-rs exited early");
        std::thread::sleep(Duration::from_millis(10));
    };

    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert_eq!(stderr, "\nAborted!\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("downloaded fw-base"), "stdout: {stdout}");

    assert!(disconnected.recv_timeout(Duration::from_secs(5)).unwrap());
    server.join().unwrap();
    assert!(!partial.exists(), "partial temp file left behind");
    assert!(files_in(&repo.join("blobs")).is_empty());
    assert!(!repo.join("refs").exists());
    assert!(files_in(&repo.join("snapshots").join(SHA)).is_empty());
}
