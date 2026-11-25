//! Rewriting a saved transcript's speaker labels in place
//! (`meetings._relabel_text` / `relabel_transcript`,
//! `meeting_client.vault_notes_path`).
//!
//! Only `**MM:SS** **<old>:** ` line prefixes flagged by the sidecar and the
//! frontmatter `speakers:` entry change; the raw-notes tail and every other
//! byte stay. Only the transcript file itself is ever opened for writing.

use super::random_token;
use lf_core::symbols::pycompat as py;
use regex::{Captures, Regex};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, FileTimes},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[derive(Debug)]
pub enum RelabelError {
    /// The transcript is not the file this edit was computed for; nothing was written.
    Changed(&'static str),
    NotFound,
    /// Not UTF-8.
    NotReadable,
    Io(io::Error),
}
impl fmt::Display for RelabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Changed(message) => f.write_str(message),
            Self::NotFound => f.write_str("transcript file not found"),
            Self::NotReadable => f.write_str("transcript is not readable"),
            Self::Io(_) => f.write_str("could not rewrite the transcript"),
        }
    }
}
impl std::error::Error for RelabelError {}
impl From<io::Error> for RelabelError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(error)
        }
    }
}

/// Byte offsets where a `re.MULTILINE` `^` matches at or after `from`.
fn line_starts(text: &str, from: usize) -> impl Iterator<Item = usize> + '_ {
    (from..=text.len())
        .filter(move |&i| text.is_char_boundary(i) && (i == 0 || text.as_bytes()[i - 1] == b'\n'))
}
fn line_at(text: &str, start: usize) -> &str {
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    &text[start..end]
}

/// End of the first `^<marker>\r?$` line at or after `from`.
fn find_line(text: &str, from: usize, marker: &str) -> Option<(usize, usize)> {
    line_starts(text, from).find_map(|start| {
        let line = line_at(text, start);
        (line == marker || line.strip_suffix('\r') == Some(marker))
            .then(|| (start, start + line.len()))
    })
}

/// `meeting_id:[ \t]*("id"|'id'|id)\s*` as a full line (after `rstrip("\r")`).
fn is_meeting_id_line(line: &str, meeting_id: &str) -> bool {
    let line = line.trim_end_matches('\r');
    let Some(rest) = line.strip_prefix("meeting_id:") else {
        return false;
    };
    let rest = rest.trim_start_matches([' ', '\t']);
    let rest = [
        format!("\"{meeting_id}\""),
        format!("'{meeting_id}'"),
        meeting_id.to_string(),
    ]
    .iter()
    .find_map(|form| rest.strip_prefix(form.as_str()))
    .map(str::to_owned);
    rest.is_some_and(|r| r.chars().all(py::is_space))
}

/// `[start, end)` of the frontmatter `speakers:` entry, flow (`speakers: [...]`)
/// or block style (`speakers:` then `- item` lines, with blank and `#` lines
/// between items), exactly as `meetings._SPEAKERS_LINE` matches it.
fn speakers_entry(head: &str) -> Option<(usize, usize)> {
    let start = line_starts(head, 0).find(|&i| head[i..].starts_with("speakers:"))?;
    let mut end = start + "speakers:".len();
    if head[end..].starts_with([' ', '\t']) {
        end += line_at(head, end)
            .find('\r')
            .unwrap_or(line_at(head, end).len());
    }
    let mut at = end;
    'items: loop {
        let mut q = at;
        loop {
            q += if head[q..].starts_with("\r\n") {
                2
            } else if head[q..].starts_with('\n') {
                1
            } else {
                break 'items;
            };
            let line = line_at(head, q);
            let at_end = q + line.len() == head.len();
            let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
            let body = &line[indent..];
            if let Some(after) = body.strip_prefix('-') {
                let content = if after.is_empty() || (after == "\r" && !at_end) {
                    Some(0)
                } else if after.starts_with([' ', '\t']) {
                    let plain = after.find('\r').unwrap_or(after.len());
                    let tail = &after[plain..];
                    (tail.is_empty() || (tail == "\r" && !at_end)).then_some(plain)
                } else {
                    None
                };
                let Some(content) = content else {
                    break 'items;
                };
                end = q + indent + 1 + content;
                at = end;
                continue 'items;
            }
            // A blank or comment line between items.
            let filler = line.strip_suffix('\r').unwrap_or(line);
            let rest = filler.trim_start_matches([' ', '\t']);
            if !(rest.is_empty() || (rest.starts_with('#') && !rest.contains('\r'))) {
                break 'items;
            }
            q += filler.len();
        }
    }
    Some((start, end))
}

/// Relabel `**MM:SS** **old:** ` lines to `new` and replace the frontmatter
/// `speakers:` entry; returns `(text, lines changed)`. `lines` maps a stamp to
/// one flag per `old`-labelled line at that stamp, in body order.
pub fn relabel_text(
    text: &str,
    lines: &BTreeMap<String, Vec<bool>>,
    old: &str,
    new: &str,
    speakers: Option<&[String]>,
    meeting_id: Option<&str>,
) -> Result<(String, usize), RelabelError> {
    let (mut head, rest) = if text.starts_with("---\n") || text.starts_with("---\r\n") {
        match find_line(text, 4, "---") {
            Some((_, end)) => (text[..end].to_string(), &text[end..]),
            None => (String::new(), text),
        }
    } else {
        (String::new(), text)
    };
    let (body, tail) = match find_line(rest, 0, "## My raw notes") {
        Some((start, _)) => (&rest[..start], &rest[start..]),
        None => (rest, ""),
    };
    if let Some(id) = meeting_id {
        if !head.split('\n').any(|line| is_meeting_id_line(line, id)) {
            return Err(RelabelError::Changed(
                "transcript does not belong to this meeting",
            ));
        }
    }
    let mut count = 0;
    let mut body = body.to_string();
    if old != new && !lines.is_empty() {
        let pattern = Regex::new(&format!(
            r"(?m)^\*\*(\d{{2,}}:\d{{2}})\*\* \*\*{}:\*\* ",
            regex::escape(old)
        ))
        .expect("valid relabel pattern");
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        body = pattern
            .replace_all(&body, |caps: &Captures<'_>| {
                let key = &caps[1];
                let index = seen.entry(key.to_string()).or_default();
                let flag = lines.get(key).and_then(|f| f.get(*index)).copied();
                *index += 1;
                if flag == Some(true) {
                    count += 1;
                    format!("**{key}** **{new}:** ")
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();
        if lines
            .iter()
            .any(|(key, flags)| seen.get(key).copied().unwrap_or(0) != flags.len())
        {
            return Err(RelabelError::Changed(
                "transcript lines do not match the speaker data",
            ));
        }
    }
    if let Some(speakers) = speakers.filter(|_| !head.is_empty()) {
        if let Some((start, end)) = speakers_entry(&head) {
            let list = Value::Array(speakers.iter().cloned().map(Value::String).collect());
            let line = format!("speakers: {}", py::json_dumps(&list, None, false));
            head.replace_range(start..end, &line);
        }
    }
    Ok((head + &body + tail, count))
}

/// Rewrite a saved transcript's speaker labels in place: atomic, mode kept,
/// atime kept, mtime moved by 2 s alternating up and down (by the parity of
/// `mtime_ns / 2e9`), and refused when the file changed meanwhile.
pub fn relabel_transcript(
    path: &Path,
    lines: &BTreeMap<String, Vec<bool>>,
    old: &str,
    new: &str,
    speakers: Option<&[String]>,
    meeting_id: Option<&str>,
) -> Result<usize, RelabelError> {
    let before = fs::metadata(path)?;
    let raw = fs::read(path)?;
    let text = std::str::from_utf8(&raw).map_err(|_| RelabelError::NotReadable)?;
    let (new_text, count) = relabel_text(text, lines, old, new, speakers, meeting_id)?;
    if new_text.as_bytes() == raw {
        return Ok(0);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.tmp", random_token()));
    let result = (|| -> Result<(), RelabelError> {
        let mut file = super::create_private(&tmp)?;
        file.write_all(new_text.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        let _ = fs::set_permissions(&tmp, before.permissions());
        let modified = before.modified()?;
        let since = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let step = Duration::from_secs(2);
        let moved = if (since / 2_000_000_000) % 2 == 0 {
            modified + step
        } else {
            modified - step
        };
        let mut times = FileTimes::new().set_modified(moved);
        if let Ok(accessed) = before.accessed() {
            times = times.set_accessed(accessed);
        }
        file.set_times(times)?;
        drop(file);
        let now = fs::metadata(path)?;
        if now.len() != before.len() || now.modified().ok() != Some(modified) {
            return Err(RelabelError::Changed(
                "transcript changed while it was being rewritten",
            ));
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| count)
}

/// The resolved file when `path` is an existing `.md` file inside the vault
/// (both resolved), else `None`. Never fails.
pub fn vault_notes_path(path: &str, vault: &str) -> Option<PathBuf> {
    if py::strip(path).is_empty() || vault.is_empty() {
        return None;
    }
    let expand = |p: &str| lf_core::home::expand_user(Path::new(p)).ok();
    let vault = fs::canonicalize(expand(vault)?).ok()?;
    let target = fs::canonicalize(expand(path)?).ok()?;
    let markdown = target
        .extension()
        .is_some_and(|e| e.to_string_lossy().to_lowercase() == "md");
    (markdown && target.is_file() && target.starts_with(&vault)).then_some(target)
}
