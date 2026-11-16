use crate::{lines, read_text, trim, words, write_text};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Local, NaiveDateTime, Timelike};
use lf_core::symbols::pycompat as py;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

pub const STATUSES: &[&str] = &["pending", "approved", "rejected", "dispatched", "failed"];
pub const APPROVE_FROM: &[&str] = &["pending", "failed"];
pub const REJECT_FROM: &[&str] = &["pending", "failed"];
pub fn default_prompts_dir() -> Result<PathBuf> {
    Ok(lf_core::home::python_home()?.join("projs/prompts/meetings"))
}
pub fn queue_path() -> Result<PathBuf> {
    Ok(lf_core::home::python_home()?.join(".localflow/queue.json"))
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPrompt {
    pub title: String,
    pub body: String,
}
pub fn parse_work_prompts(md: &str) -> Vec<WorkPrompt> {
    let mut out = Vec::new();
    let mut title = None;
    let mut body = Vec::new();
    let flush = |out: &mut Vec<WorkPrompt>, title: &mut Option<String>, body: &mut Vec<String>| {
        if let Some(title) = title.take() {
            if !body.is_empty() {
                out.push(WorkPrompt {
                    title,
                    body: body.join(" "),
                });
            }
        }
        body.clear();
    };
    for line in lines(md).map(trim) {
        if line.starts_with('#') {
            let heading = trim(line.trim_start_matches('#'));
            if !heading.is_empty() {
                flush(&mut out, &mut title, &mut body);
                title = Some(heading.into());
            }
        } else if let Some(text) = line.strip_prefix('>') {
            let text = trim(text);
            if title.is_some() && !text.is_empty() {
                body.push(text.into());
            }
        }
    }
    flush(&mut out, &mut title, &mut body);
    out
}
pub fn slugify(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut slug = String::new();
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        return "untitled".into();
    }
    if slug.len() <= 50 {
        return slug.into();
    }
    let cut = &slug[..50];
    let cut = if cut[20..].contains('-') {
        &cut[..cut.rfind('-').unwrap()]
    } else {
        cut
    };
    let cut = cut.trim_matches('-');
    if cut.is_empty() {
        slug[..50].trim_matches('-').into()
    } else {
        cut.into()
    }
}
pub fn header_value(value: &str) -> String {
    let flat: String = value
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || ['\u{7f}', '\u{85}', '\u{2028}', '\u{2029}'].contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    words(&flat)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}
pub fn strip_frontmatter(text: &str) -> String {
    let lines: Vec<_> = text.split('\n').collect();
    if lines[0].trim_end_matches('\r') != "---" {
        return text.into();
    }
    if let Some(end) = (1..lines.len()).find(|&i| lines[i].trim_end_matches('\r') == "---") {
        lines[end + 1..].join("\n").trim_start_matches('\n').into()
    } else {
        text.into()
    }
}
pub fn isoformat(stamp: NaiveDateTime) -> String {
    if stamp.nanosecond() / 1000 == 0 {
        stamp.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!(
            "{}.{:06}",
            stamp.format("%Y-%m-%dT%H:%M:%S"),
            stamp.nanosecond() / 1000
        )
    }
}
pub enum StartedAt {
    DateTime(NaiveDateTime),
    Timestamp(f64),
}
impl From<NaiveDateTime> for StartedAt {
    fn from(value: NaiveDateTime) -> Self {
        Self::DateTime(value)
    }
}
impl From<f64> for StartedAt {
    fn from(value: f64) -> Self {
        Self::Timestamp(value)
    }
}
impl StartedAt {
    fn local_datetime(self) -> io::Result<NaiveDateTime> {
        match self {
            Self::DateTime(value) => Ok(value),
            Self::Timestamp(value) => {
                let micros = (value * 1_000_000.0).round_ties_even();
                if !micros.is_finite() || micros < i64::MIN as f64 || micros >= i64::MAX as f64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "timestamp out of range",
                    ));
                }
                DateTime::from_timestamp_micros(micros as i64)
                    .map(|stamp| stamp.with_timezone(&Local).naive_local())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "timestamp out of range")
                    })
            }
        }
    }
}
pub fn write_prompts(
    prompts: &[WorkPrompt],
    meeting_title: &str,
    started_at: impl Into<StartedAt>,
    prompts_dir: &Path,
) -> io::Result<Vec<PathBuf>> {
    if prompts.is_empty() {
        return Ok(Vec::new());
    }
    let started_at = started_at.into().local_datetime()?;
    let folder = prompts_dir.join(format!(
        "{}-{}",
        started_at.format("%Y-%m-%d"),
        slugify(meeting_title)
    ));
    fs::create_dir_all(&folder)?;
    let mut out = Vec::new();
    for (i, prompt) in prompts.iter().enumerate() {
        let path = folder.join(format!("{:02}-{}.md", i + 1, slugify(&prompt.title)));
        write_text(
            &path,
            &format!(
                "---\ntitle: {}\nmeeting: {}\ncreated: {}\nstatus: pending\n---\n\n{}\n",
                header_value(&prompt.title),
                header_value(meeting_title),
                isoformat(started_at),
                prompt.body
            ),
        )?;
        out.push(path);
    }
    Ok(out)
}
pub fn entry_id(path: &Path, title: &str) -> String {
    let digest = Sha256::digest(format!("{}\n{title}", path_text(path)).as_bytes());
    format!("{digest:x}")[..8].into()
}
fn path_text(path: &Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().replace('/', "\\");
        use std::path::Component;
        let mut normalized = String::new();
        let mut has_part = false;
        for component in Path::new(&text).components() {
            match component {
                Component::Prefix(prefix) => {
                    normalized.push_str(&prefix.as_os_str().to_string_lossy())
                }
                Component::RootDir => normalized.push('\\'),
                Component::CurDir => (),
                Component::Normal(_) | Component::ParentDir => {
                    if has_part && !normalized.ends_with('\\') {
                        normalized.push('\\');
                    }
                    normalized.push_str(&component.as_os_str().to_string_lossy());
                    has_part = true;
                }
            }
        }
        if normalized.is_empty() {
            ".".into()
        } else {
            normalized
        }
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into()
    }
}

#[derive(Debug)]
pub struct StatusConflict {
    pub entry_id: String,
    pub current: String,
    pub expected: Vec<String>,
}
impl std::fmt::Display for StatusConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot move {} from {}; expected one of {}",
            self.entry_id,
            py::repr_str(&self.current),
            self.expected.join(", ")
        )
    }
}
impl std::error::Error for StatusConflict {}
pub struct PromptQueue {
    pub path: PathBuf,
    lock: Mutex<()>,
}
impl PromptQueue {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }
    pub fn at_default_path() -> Result<Self> {
        Ok(Self::new(queue_path()?))
    }
    fn exclusive<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _thread = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        let _file = py::SidecarLock::acquire(
            &PathBuf::from(format!("{}.lock", self.path.display())),
            self.path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            "queue",
        );
        f()
    }
    fn read(&self) -> Vec<Value> {
        read_text(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }
    fn write(&self, entries: &[Value]) -> Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let tmp = PathBuf::from(format!(
            "{}.{}.tmp",
            self.path.display(),
            std::process::id()
        ));
        write_text(&tmp, &py::json_dumps(&json!(entries), Some(2), true))?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
    pub fn add(
        &self,
        prompts: &[WorkPrompt],
        paths: &[PathBuf],
        meeting_title: &str,
    ) -> Result<Vec<Value>> {
        self.add_at(prompts, paths, meeting_title, Local::now().naive_local())
    }
    pub fn add_at(
        &self,
        prompts: &[WorkPrompt],
        paths: &[PathBuf],
        meeting_title: &str,
        now: NaiveDateTime,
    ) -> Result<Vec<Value>> {
        self.exclusive(|| {
            let mut entries = self.read();
            let mut created = Vec::new();
            for (prompt, path) in prompts.iter().zip(paths) {
                let id = entry_id(path, &prompt.title);
                if let Some(existing) = entries.iter().rev().find(|e| e.get("id") == Some(&json!(id))) {
                    created.push(existing.clone());
                    continue;
                }
                let entry = json!({"id":id,"title":prompt.title,"path":path_text(path),"meeting":meeting_title,"created":isoformat(now),"status":"pending","worktree":"","handle":"","repo":"","error":""});
                entries.push(entry.clone());
                created.push(entry);
            }
            self.write(&entries)?;
            Ok(created)
        })
    }
    pub fn list(&self, status: Option<&str>) -> Result<Vec<Value>> {
        self.exclusive(|| {
            Ok(self
                .read()
                .into_iter()
                .filter(|e| status.is_none_or(|s| e.get("status") == Some(&json!(s))))
                .collect())
        })
    }
    pub fn get(&self, id: &str) -> Result<Option<Value>> {
        self.exclusive(|| {
            Ok(self
                .read()
                .into_iter()
                .find(|e| e.get("id") == Some(&json!(id))))
        })
    }
    pub fn set_status(
        &self,
        id: &str,
        status: &str,
        expect: Option<&[&str]>,
        fields: &Map<String, Value>,
    ) -> Result<Option<Value>> {
        if !STATUSES.contains(&status) {
            bail!("unknown status {}; expected one of approved, dispatched, failed, pending, rejected",py::repr_str(status));
        }
        self.exclusive(|| {
            let mut entries = self.read();
            for entry in &mut entries {
                if entry.get("id") != Some(&json!(id)) {
                    continue;
                }
                let current = entry.get("status").and_then(Value::as_str).unwrap_or("");
                if let Some(expected) = expect {
                    if !expected.contains(&current) {
                        return Err(StatusConflict {
                            entry_id: id.into(),
                            current: current.into(),
                            expected: expected.iter().map(|s| (*s).into()).collect(),
                        }
                        .into());
                    }
                }
                entry["status"] = json!(status);
                if status == "approved" {
                    for key in ["error", "worktree", "handle"] {
                        entry[key] = json!("");
                    }
                }
                entry
                    .as_object_mut()
                    .ok_or_else(|| anyhow!("invalid queue entry"))?
                    .extend(fields.clone());
                let result = entry.clone();
                self.write(&entries)?;
                return Ok(Some(result));
            }
            Ok(None)
        })
    }
}
#[derive(Debug)]
pub struct PromptTransitionError {
    pub status_code: u16,
    pub detail: String,
}
impl std::fmt::Display for PromptTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl std::error::Error for PromptTransitionError {}
pub fn transition_prompt(
    queue: &PromptQueue,
    id: &str,
    verb: &str,
    status: &str,
    allowed: &[&str],
) -> Result<Value> {
    match queue.set_status(id, status, Some(allowed), &Map::new()) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(PromptTransitionError {
            status_code: 404,
            detail: format!("unknown prompt: {id}"),
        }
        .into()),
        Err(e) => {
            if let Some(conflict) = e.downcast_ref::<StatusConflict>() {
                Err(PromptTransitionError {
                    status_code: 409,
                    detail: format!(
                        "cannot {verb} a {} prompt (allowed from: {})",
                        conflict.current,
                        allowed.join(", ")
                    ),
                }
                .into())
            } else {
                Err(e)
            }
        }
    }
}
pub fn approve_prompt(queue: &PromptQueue, id: &str) -> Result<Value> {
    transition_prompt(queue, id, "approve", "approved", APPROVE_FROM)
}
pub fn reject_prompt(queue: &PromptQueue, id: &str) -> Result<Value> {
    transition_prompt(queue, id, "reject", "rejected", REJECT_FROM)
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}
#[derive(Debug)]
pub enum CommandError {
    NotFound,
    Timeout,
    Other(String),
}
pub trait CommandRunner {
    fn run(
        &self,
        argv: &[String],
        timeout: Duration,
    ) -> std::result::Result<CommandOutput, CommandError>;
}
pub struct ProcessRunner;
impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        argv: &[String],
        timeout: Duration,
    ) -> std::result::Result<CommandOutput, CommandError> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    CommandError::NotFound
                } else {
                    CommandError::Other(e.to_string())
                }
            })?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let err = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let start = Instant::now();
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => (),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandError::Other(e.to_string()));
                }
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CommandError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let decode = |handle: std::thread::JoinHandle<io::Result<Vec<u8>>>| {
            handle
                .join()
                .map_err(|_| CommandError::Other("output reader failed".into()))?
                .map_err(|e| CommandError::Other(e.to_string()))
                .and_then(|bytes| {
                    String::from_utf8(bytes)
                        .map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
                        .map_err(|e| CommandError::Other(e.to_string()))
                })
        };
        #[cfg(unix)]
        let returncode = {
            use std::os::unix::process::ExitStatusExt;
            code.code().unwrap_or_else(|| -code.signal().unwrap_or(1))
        };
        #[cfg(not(unix))]
        let returncode = code.code().unwrap_or(1);
        Ok(CommandOutput {
            returncode,
            stdout: decode(out)?,
            stderr: decode(err)?,
        })
    }
}
pub fn parse_json_stdout(stdout: &str) -> Result<Value> {
    let start = stdout
        .find('{')
        .ok_or_else(|| anyhow!("no JSON in output"))?;
    Ok(serde_json::from_str(&stdout[start..])?)
}
pub fn orca_available() -> (bool, String) {
    let ssh = ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|s| !s.is_empty()));
    orca_available_with(&ProcessRunner, ssh)
}
pub fn orca_available_with(runner: &dyn CommandRunner, headless: bool) -> (bool, String) {
    let argv = ["orca", "status", "--json"].map(String::from);
    let proc = match runner.run(&argv, Duration::from_secs(15)) {
        Ok(p) => p,
        Err(e) => {
            return (
                false,
                match e {
                    CommandError::NotFound => "orca CLI not found on PATH".into(),
                    CommandError::Timeout => "orca status timed out".into(),
                    CommandError::Other(e) => format!("orca status failed: {e}"),
                },
            )
        }
    };
    if proc.returncode != 0 {
        return (false, format!("orca status exited {}", proc.returncode));
    }
    let data = match parse_json_stdout(&proc.stdout) {
        Ok(data) => data,
        Err(e) => return (false, format!("unreadable orca status output: {e}")),
    };
    let result = &data["result"];
    let note = if headless {
        " (not possible over SSH: it launches a GUI app on the Mac's own display)"
    } else {
        ""
    };
    if !truthy(&result["app"]["running"]) {
        return (
            false,
            format!("Orca app is not running — run `orca open`{note}"),
        );
    }
    if !truthy(&result["runtime"]["reachable"]) {
        let state = result["runtime"]["state"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        return (
            false,
            format!("Orca runtime not reachable ({state}) — run `orca open`{note}"),
        );
    }
    (true, String::new())
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
pub fn last_line(text: &str) -> String {
    lines(text)
        .filter(|s| !trim(s).is_empty())
        .last()
        .map(|s| trim(s).chars().take(200).collect())
        .unwrap_or("no output".into())
}
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => py::repr_str(s),
        Value::Number(n) if n.is_f64() => py::float_repr(n.as_f64().unwrap()),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", py::repr_str(k), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
pub fn worktree_id(data: &Value, fallback: &str) -> String {
    let result = &data["result"];
    if result.is_object() {
        let source = if result["worktree"].is_object() {
            &result["worktree"]
        } else {
            result
        };
        for key in ["worktreeId", "id", "path", "name", "branch"] {
            if let Some(s) = source[key].as_str().filter(|s| !s.is_empty()) {
                return s.into();
            }
        }
    }
    fallback.into()
}
pub fn terminal_handle(data: &Value) -> String {
    let result = &data["result"];
    result["agentTerminalHandle"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| result["startupTerminal"]["handle"].as_str())
        .unwrap_or("")
        .into()
}
pub fn dispatch(entry: &Value, repo: &str, agent: &str) -> Value {
    dispatch_with(entry, repo, agent, &ProcessRunner)
}
pub fn dispatch_with(entry: &Value, repo: &str, agent: &str, runner: &dyn CommandRunner) -> Value {
    let path = Path::new(entry["path"].as_str().unwrap_or(""));
    let body = match read_text(if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }) {
        Ok(s) => trim(&strip_frontmatter(&s)).to_string(),
        Err(e) => {
            return json!({"ok":false,"error":format!("cannot read prompt file {}: {e}",if path.as_os_str().is_empty(){".".into()}else{path.display().to_string()})})
        }
    };
    if body.is_empty() {
        return json!({"ok":false,"error":format!("prompt file is empty: {}",path.display())});
    }
    let slug = slugify(entry["title"].as_str().unwrap_or(""));
    let name = slug[..slug.len().min(40)].trim_matches('-');
    let name = if name.is_empty() { "untitled" } else { name };
    let argv = [
        "orca",
        "worktree",
        "create",
        "--repo",
        repo,
        "--name",
        name,
        "--agent",
        agent,
        "--prompt",
        &body,
        "--no-parent",
        "--setup",
        "run",
        "--json",
    ]
    .map(String::from);
    let proc = match runner.run(&argv, Duration::from_secs(120)) {
        Ok(p) => p,
        Err(e) => {
            return json!({"ok":false,"error":match e{CommandError::NotFound=>"orca CLI not found on PATH".into(),CommandError::Timeout=>"orca worktree create timed out after 120s".into(),CommandError::Other(e)=>format!("orca worktree create failed: {e}")}})
        }
    };
    if proc.returncode != 0 {
        return json!({"ok":false,"error":format!("orca exited {}: {}",proc.returncode,last_line(&proc.stderr))});
    }
    let data = match parse_json_stdout(&proc.stdout) {
        Ok(d) => d,
        Err(e) => return json!({"ok":false,"error":format!("unreadable orca output: {e}")}),
    };
    if data.get("ok") == Some(&Value::Bool(false)) {
        let error = if truthy(&data["error"]) {
            data["error"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| python_repr(&data["error"]))
        } else {
            python_repr(&data)
        };
        return json!({"ok":false,"error":format!("orca reported failure: {}",last_line(&error))});
    }
    json!({"ok":true,"worktree":worktree_id(&data,name),"handle":terminal_handle(&data),"raw":data})
}
