//! Framed speech IPC and ownership of a same-executable model process.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_core::config::Config;
use lf_runtime::engine::SpeechEngine;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

const MAX_HEADER: usize = 1024 * 1024;
const MAX_SAMPLES: usize = 16000 * 60 * 60;
const STDERR_LIMIT: usize = 8192;
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Load {
        fields: BTreeMap<String, serde_json::Value>,
    },
    /// Load only an already installed model, never downloading it. A
    /// separate op (not a field on `Load`, which serde would silently ignore):
    /// a helper built before it existed can't parse the request and exits
    /// before loading anything.
    LoadCached {
        fields: BTreeMap<String, serde_json::Value>,
    },
    Transcribe {
        samples: usize,
        prompt: Option<String>,
    },
    Unload,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum Reply {
    Ready,
    Text(String),
    Error(String),
}

fn write_header(writer: &mut impl Write, header: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(header)?;
    if bytes.len() > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "speech header too large",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)
}

fn read_header<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut size = [0; 4];
    if reader.read(&mut size[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut size[1..])?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid speech header length",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn write_pcm(writer: &mut impl Write, pcm: &[f32]) -> io::Result<()> {
    let mut bytes = [0; 4096];
    for chunk in pcm.chunks(bytes.len() / 4) {
        for (sample, target) in chunk.iter().zip(bytes.as_chunks_mut::<4>().0.iter_mut()) {
            target.copy_from_slice(&sample.to_le_bytes());
        }
        writer.write_all(&bytes[..chunk.len() * 4])?;
    }
    Ok(())
}

fn read_pcm(reader: &mut impl Read, samples: usize) -> io::Result<Vec<f32>> {
    if samples > MAX_SAMPLES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "speech PCM too large",
        ));
    }
    let mut pcm = Vec::with_capacity(samples);
    let mut bytes = [0; 4096];
    while pcm.len() < samples {
        let count = (samples - pcm.len()).min(bytes.len() / 4);
        reader.read_exact(&mut bytes[..count * 4])?;
        for sample in bytes[..count * 4].as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*sample);
            if !value.is_finite() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "non-finite speech PCM",
                ));
            }
            pcm.push(value);
        }
    }
    Ok(pcm)
}

struct Pipes {
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    ready: bool,
}

struct Session {
    child: Mutex<Child>,
    pipes: Mutex<Pipes>,
    stopped: AtomicBool,
    ready: AtomicBool,
    tail: Arc<Mutex<VecDeque<u8>>>,
    drain: Mutex<Option<JoinHandle<()>>>,
}

fn drain_stderr(mut input: impl Read, mut output: impl Write, sink: &Mutex<VecDeque<u8>>) {
    let mut bytes = [0; 1024];
    loop {
        let count = match input.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        {
            let mut tail = sink.lock().unwrap_or_else(|p| p.into_inner());
            tail.extend(&bytes[..count]);
            let extra = tail.len().saturating_sub(STDERR_LIMIT);
            tail.drain(..extra);
        }
        let _ = output.write_all(&bytes[..count]);
    }
}

impl Session {
    fn spawn(exe: &std::path::Path) -> Result<Self, String> {
        let mut child = Command::new(exe)
            .arg("--speech-helper")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn speech helper: {error}"))?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stderr = child.stderr.take().expect("piped stderr");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let sink = tail.clone();
        let drain = std::thread::Builder::new()
            .name("speech-stderr".into())
            .spawn(move || drain_stderr(stderr, io::stderr(), &sink));
        let drain = match drain {
            Ok(drain) => drain,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                crate::stt_lease::release_process(child.id());
                return Err(format!("capture speech helper stderr: {error}"));
            }
        };
        Ok(Self {
            child: Mutex::new(child),
            pipes: Mutex::new(Pipes {
                input,
                output,
                ready: false,
            }),
            stopped: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            tail,
            drain: Mutex::new(Some(drain)),
        })
    }

    fn alive(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && matches!(
                self.child
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .try_wait(),
                Ok(None)
            )
    }

    fn call(&self, load: &Request, pcm: Option<(&[f32], Option<&str>)>) -> Result<String, String> {
        let mut pipes = self
            .pipes
            .lock()
            .map_err(|_| "speech helper pipes poisoned")?;
        if self.stopped.load(Ordering::Acquire) {
            return Err("speech helper stopped".into());
        }
        let exchange = |pipes: &mut Pipes, request: &Request, pcm: &[f32]| -> io::Result<Reply> {
            let input = pipes
                .input
                .as_mut()
                .ok_or_else(|| io::Error::other("speech pipe closed"))?;
            write_header(input, request)?;
            write_pcm(input, pcm)?;
            input.flush()?;
            read_header(&mut pipes.output)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "speech helper closed stdout")
            })
        };
        if !pipes.ready {
            match exchange(&mut pipes, load, &[]).map_err(|e| e.to_string())? {
                Reply::Ready => {
                    pipes.ready = true;
                    self.ready.store(true, Ordering::Release);
                }
                Reply::Error(error) => return Err(error),
                _ => return Err("speech helper did not send ready".into()),
            }
        }
        if let Some((pcm, prompt)) = pcm {
            if pcm.len() > MAX_SAMPLES || pcm.iter().any(|s| !s.is_finite()) {
                return Err("invalid speech PCM".into());
            }
            let request = Request::Transcribe {
                samples: pcm.len(),
                prompt: prompt.map(str::to_owned),
            };
            match exchange(&mut pipes, &request, pcm).map_err(|e| e.to_string())? {
                Reply::Text(text) => Ok(text),
                Reply::Error(error) => Err(error),
                _ => Err("unexpected speech helper reply".into()),
            }
        } else {
            Ok(String::new())
        }
    }

    fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.ready.store(false, Ordering::Release);
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut sent = false;
        loop {
            if !sent {
                if let Ok(mut pipes) = self.pipes.try_lock() {
                    if let Some(input) = pipes.input.as_mut() {
                        if write_header(input, &Request::Unload)
                            .and_then(|()| input.flush())
                            .is_err()
                        {
                            pipes.input.take();
                        }
                    }
                    sent = true;
                }
            }
            let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            drop(child);
            std::thread::sleep(Duration::from_millis(10));
        }
        crate::stt_lease::release_process(
            self.child.lock().unwrap_or_else(|p| p.into_inner()).id(),
        );
        if let Some(drain) = self.drain.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = drain.join();
        }
    }

    fn stderr(&self) -> String {
        let bytes: Vec<_> = self
            .tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

pub struct HelperSpeechEngine {
    executable: PathBuf,
    /// The first request of every helper session (`Load` or `LoadCached`).
    load: Request,
    session: Mutex<Option<Arc<Session>>>,
}

impl HelperSpeechEngine {
    pub fn new(executable: PathBuf, config: &Config) -> Self {
        let fields = Self::fields(config);
        Self::with_load(executable, Request::Load { fields })
    }

    /// A helper that may only load the already installed configured model.
    /// Needs a helper served by [`serve_stdio_with`]; any other helper,
    /// including one built before cache-only loading existed, fails the
    /// load before it reads or downloads a model.
    pub fn cached(executable: PathBuf, config: &Config) -> Self {
        let fields = Self::fields(config);
        Self::with_load(executable, Request::LoadCached { fields })
    }

    fn with_load(executable: PathBuf, load: Request) -> Self {
        Self {
            executable,
            load,
            session: Mutex::new(None),
        }
    }

    fn fields(config: &Config) -> BTreeMap<String, serde_json::Value> {
        [
            "stt_backend",
            "model_size",
            "language",
            "stt_device",
            "sample_rate",
        ]
        .into_iter()
        .filter_map(|key| {
            config
                .fields
                .get(key)
                .map(|value| (key.to_owned(), value.clone()))
        })
        .collect()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .filter(|session| session.alive())
            .map(|session| session.child.lock().unwrap_or_else(|p| p.into_inner()).id())
    }

    fn call(&self, pcm: Option<(&[f32], Option<&str>)>) -> Result<String, String> {
        let session = {
            let mut slot = self
                .session
                .lock()
                .map_err(|_| "speech helper state poisoned")?;
            if slot.as_ref().is_some_and(|session| !session.alive()) {
                slot.take().unwrap().stop();
            }
            if slot.is_none() {
                *slot = Some(Arc::new(Session::spawn(&self.executable)?));
            }
            slot.as_ref().unwrap().clone()
        };
        match session.call(&self.load, pcm) {
            Ok(text) => Ok(text),
            Err(error) => {
                let mut slot = self.session.lock().unwrap_or_else(|p| p.into_inner());
                session.stop();
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &session))
                {
                    slot.take();
                }
                let tail = session.stderr();
                Err(format!("speech helper: {error}; stderr tail: {tail}"))
            }
        }
    }
}

impl SpeechEngine for HelperSpeechEngine {
    fn loaded(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|session| session.ready.load(Ordering::Acquire) && session.alive())
    }
    fn load(&self) -> Result<(), String> {
        self.call(None).map(|_| ())
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.call(Some((pcm, prompt)))
    }
    fn unload(&self) {
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            session.stop();
        }
    }
}

impl Drop for HelperSpeechEngine {
    fn drop(&mut self) {
        self.unload();
    }
}

#[cfg(unix)]
fn protocol_stdout() -> io::Result<Box<dyn Write>> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: dup returns an owned descriptor; stdout/stderr remain process-owned.
    let fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh, valid descriptor owned only here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    use std::os::fd::AsRawFd;
    // SAFETY: these operations change descriptor flags/routing, not Rust-owned memory.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Box::new(std::fs::File::from(fd)))
}

#[cfg(not(unix))]
fn protocol_stdout() -> io::Result<Box<dyn Write>> {
    Ok(Box::new(io::stdout()))
}

/// Only call in the dedicated helper process: EOF terminates even a busy engine.
/// A cache-only load request is refused before `make` runs (see
/// [`serve_stdio_with`] for a helper that supports it).
pub fn serve_stdio(
    make: impl FnOnce(Config) -> Result<Arc<dyn SpeechEngine>, String>,
) -> Result<(), String> {
    serve_stdio_with(|config, cache_only| {
        if cache_only {
            Err(CACHE_ONLY_UNSUPPORTED.into())
        } else {
            make(config)
        }
    })
}

/// Why a helper refused a cache-only load.
pub const CACHE_ONLY_UNSUPPORTED: &str =
    "this speech helper can't load an installed model without downloading; nothing was loaded";

/// [`serve_stdio`] whose `make` also receives whether the parent asked for
/// a cache-only load (`HelperSpeechEngine::cached`); `make` must then never
/// download.
pub fn serve_stdio_with(
    make: impl FnOnce(Config, bool) -> Result<Arc<dyn SpeechEngine>, String>,
) -> Result<(), String> {
    let mut output = protocol_stdout().map_err(|e| e.to_string())?;
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let request: Request = match read_header(&mut input) {
                Ok(Some(request)) => request,
                Ok(None) => exit_helper(0),
                Err(error) => {
                    eprintln!("speech helper input: {error}");
                    exit_helper(1);
                }
            };
            let pcm = if let Request::Transcribe { samples, .. } = &request {
                match read_pcm(&mut input, *samples) {
                    Ok(pcm) => pcm,
                    Err(error) => {
                        eprintln!("speech helper PCM: {error}");
                        exit_helper(1);
                    }
                }
            } else {
                Vec::new()
            };
            if send.send((request, pcm)).is_err() {
                return;
            }
        }
    });
    let reply = |output: &mut Box<dyn Write>, reply: Reply| -> Result<(), String> {
        write_header(output, &reply)
            .and_then(|()| output.flush())
            .map_err(|e| e.to_string())
    };
    let (fields, cache_only) = match recv.recv().map_err(|e| e.to_string())? {
        (Request::Load { fields }, _) => (fields, false),
        (Request::LoadCached { fields }, _) => (fields, true),
        _ => return Err("expected speech load".into()),
    };
    let mut config = Config::default();
    config.fields.extend(fields);
    let engine = match make(config, cache_only).and_then(|engine| engine.load().map(|()| engine)) {
        Ok(engine) => engine,
        Err(error) => {
            reply(&mut output, Reply::Error(error))?;
            return Ok(());
        }
    };
    reply(&mut output, Reply::Ready)?;
    while let Ok((request, pcm)) = recv.recv() {
        match request {
            Request::Unload => return Ok(()),
            Request::Transcribe { prompt, .. } => {
                reply(
                    &mut output,
                    match engine.transcribe(&pcm, prompt.as_deref()) {
                        Ok(text) => Reply::Text(text),
                        Err(error) => Reply::Error(error),
                    },
                )?;
            }
            Request::Load { .. } | Request::LoadCached { .. } => {
                return Err("speech model already configured".into())
            }
        }
    }
    Ok(())
}

fn exit_helper(code: i32) -> ! {
    crate::stt_lease::release_process(std::process::id());
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_stderr_is_forwarded_verbatim_and_retains_only_bounded_tail() {
        for input in [
            vec![],
            b"warning: h\xc3\xa9llo\npartial".to_vec(),
            vec![b'x'; 20000],
        ] {
            let tail = Mutex::new(VecDeque::new());
            let mut output = Vec::new();
            drain_stderr(input.as_slice(), &mut output, &tail);
            assert_eq!(output, input);
            let retained: Vec<_> = tail.lock().unwrap().iter().copied().collect();
            assert_eq!(retained, input[input.len().saturating_sub(STDERR_LIMIT)..]);
        }
    }

    #[test]
    fn helper_stderr_capture_survives_parent_stderr_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let input = vec![b'x'; STDERR_LIMIT * 3];
        let tail = Mutex::new(VecDeque::new());
        drain_stderr(input.as_slice(), Broken, &tail);
        assert_eq!(tail.lock().unwrap().len(), STDERR_LIMIT);
    }

    /// The request enum as helpers built before `LoadCached` declared it.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum OldRequest {
        Load {
            fields: BTreeMap<String, serde_json::Value>,
        },
        Transcribe {
            samples: usize,
            prompt: Option<String>,
        },
        Unload,
    }

    #[test]
    fn cache_only_load_is_unreadable_to_old_helpers_and_plain_load_is_unchanged() {
        let config = {
            let mut config = Config::default();
            config.set_field("model_size", serde_json::json!("base"));
            config.set_field("profile", serde_json::json!("not forwarded"));
            config
        };
        let frame = |engine: &HelperSpeechEngine| {
            let mut bytes = Vec::new();
            write_header(&mut bytes, &engine.load).unwrap();
            bytes
        };
        let exe = PathBuf::from("unused");
        let plain = frame(&HelperSpeechEngine::new(exe.clone(), &config));
        match read_header::<OldRequest>(&mut plain.as_slice()).unwrap() {
            Some(OldRequest::Load { fields }) => {
                assert_eq!(fields["model_size"], "base");
                assert!(!fields.contains_key("profile"), "only speech fields");
            }
            other => panic!("{other:?}"),
        }
        let cached = frame(&HelperSpeechEngine::cached(exe, &config));
        // An old helper's input reader fails here and exits before `make`.
        assert!(read_header::<OldRequest>(&mut cached.as_slice()).is_err());
        assert!(matches!(
            read_header::<Request>(&mut cached.as_slice()).unwrap(),
            Some(Request::LoadCached { fields })
                if fields["model_size"] == "base" && !fields.contains_key("profile")
        ));
    }

    #[test]
    fn frame_round_trip_and_rejects_truncation_limits_and_invalid_samples() {
        let mut bytes = Vec::new();
        write_header(&mut bytes, &Reply::Text("héllo\n世界".into())).unwrap();
        assert_eq!(
            read_header::<Reply>(&mut bytes.as_slice()).unwrap(),
            Some(Reply::Text("héllo\n世界".into()))
        );
        assert!(read_header::<Reply>(&mut &[][..]).unwrap().is_none());
        for length in 1..bytes.len() {
            assert!(read_header::<Reply>(&mut &bytes[..length]).is_err());
        }
        for size in [0, MAX_HEADER as u32 + 1] {
            assert!(read_header::<Reply>(&mut size.to_le_bytes().as_slice()).is_err());
        }
        assert!(write_header(&mut Vec::new(), &"x".repeat(MAX_HEADER)).is_err());
        let pcm = [0.0, -0.25, 1.0];
        let mut bytes = Vec::new();
        write_pcm(&mut bytes, &pcm).unwrap();
        assert_eq!(read_pcm(&mut bytes.as_slice(), pcm.len()).unwrap(), pcm);
        assert!(read_pcm(&mut bytes.as_slice(), pcm.len() + 1).is_err());
        assert!(read_pcm(&mut &[][..], MAX_SAMPLES + 1).is_err());
        assert!(read_pcm(&mut f32::NAN.to_le_bytes().as_slice(), 1).is_err());
        assert!(read_pcm(&mut &[][..], 0).unwrap().is_empty());
    }
}
