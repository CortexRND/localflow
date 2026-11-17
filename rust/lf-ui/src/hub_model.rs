//! AppKit-free logic behind the native Hub pages (port of
//! `localflow/hub_model.py`).
//!
//! Kept separate from the windowing layer so the rules (dictionary
//! validation, prompt review, responsive sizing, recent notes) can be
//! unit-tested on any platform. Every measurement that depends on a real font
//! goes through an injected [`TextMeasure`] closure
//! (`(text, font size, weight) -> width`).
//!
//! Integer geometry uses `i32` with Python's floor division where it matters;
//! frames are `(x, y, w, h)`.

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use lf_core::config::Config;
use lf_core::dictionary::{DictionaryData, DictionaryError, DictionaryStore};
use lf_core::symbols::pycompat::{is_decimal, is_space, parse_decimal};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::backends::prompts::{
    approve_prompt, reject_prompt, PromptEntry, PromptQueue, TransitionFailure, APPROVE_FROM,
    REJECT_FROM,
};
use crate::pystr::PyStr;
pub use crate::text_wrap::wrap_lines;
use crate::text_wrap::TextMeasure;
use crate::tokens::{Weight, HUB_HEADER_OFFSET, INPUT_HEIGHT};

/// `(x, y, w, h)` in whole points.
pub type Frame = (i32, i32, i32, i32);

/// Python `a // b` for a positive `b`.
fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

/// Python `str.title()` (first letter of each cased run upper, rest lower).
fn py_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_cased = false;
    for c in text.chars() {
        if prev_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        prev_cased = c.is_lowercase() || c.is_uppercase();
    }
    out
}

/// A non-empty string field of an entry, like Python `entry.get(key) or ...`
/// (numbers stringify; other types are treated as absent).
fn text_field(entry: &Map<String, Value>, key: &str) -> Option<String> {
    match entry.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ------------------------------------------------------------------ sidebar ---

/// Width below which the Home stat cards wrap from four columns into two.
pub const STAT_FOUR_COLUMN_MIN_CARD: i32 = 150;
pub const STAT_GAP: i32 = 12;

/// Display labels only: routing keeps the page ids.
pub const SIDEBAR_LABELS: [(&str, &str); 1] = [("Models", "Models & keys")];
pub const SIDEBAR_BUTTON_WIDTH: i32 = 168;
/// Room the borderless button cell keeps around its title (both sides).
pub const SIDEBAR_TITLE_INSET: i32 = 16;
/// Two U+2002 EN SPACE characters, exactly as `hub_model.SIDEBAR_TITLE_PREFIX`.
pub const SIDEBAR_TITLE_PREFIX: &str = "\u{2002}\u{2002}";
/// Font of the sidebar buttons: 13pt semibold.
pub const SIDEBAR_FONT_SIZE: f64 = 13.0;
pub const SIDEBAR_FONT_WEIGHT: Weight = Weight::Semibold;

/// What the sidebar button and the page title say for `page`.
pub fn sidebar_label(page: &str) -> &str {
    SIDEBAR_LABELS
        .iter()
        .find(|(id, _)| *id == page)
        .map_or(page, |(_, label)| label)
}

/// The sidebar button title; the plain page id if the label would not fit.
///
/// `measure` is the real font measurement (13pt semibold) in the AppKit Hub;
/// without it the label is used as is.
pub fn sidebar_title(page: &str, measure: Option<TextMeasure<'_>>, width: i32) -> String {
    let title = format!("{SIDEBAR_TITLE_PREFIX}{}", sidebar_label(page));
    if let Some(measure) = measure {
        if measure(&title, SIDEBAR_FONT_SIZE, SIDEBAR_FONT_WEIGHT)
            > f64::from(width - SIDEBAR_TITLE_INSET)
        {
            return format!("{SIDEBAR_TITLE_PREFIX}{page}");
        }
    }
    title
}

// --------------------------------------------------------------------- home ---

pub const HOME_DECORATION_WIDTH: i32 = 150;
/// Between the decoration and the stat cards below it.
pub const HOME_DECORATION_GAP: i32 = 8;

/// The halftone's frame: the right end of the header band only, ending above
/// the stat cards (which start at `HUB_HEADER_OFFSET`).
pub fn home_decoration_frame(width: i32) -> Frame {
    (
        width - HOME_DECORATION_WIDTH,
        0,
        HOME_DECORATION_WIDTH,
        HUB_HEADER_OFFSET - HOME_DECORATION_GAP,
    )
}

/// `(columns, card_width)` for `count` stat cards across `width` points.
pub fn stat_layout(width: i32, count: i32) -> (i32, i32) {
    let mut columns = count.max(1);
    while columns > 1
        && floor_div(width - STAT_GAP * (columns - 1), columns) < STAT_FOUR_COLUMN_MIN_CARD
    {
        columns /= 2;
    }
    (
        columns,
        floor_div(width - STAT_GAP * (columns - 1), columns),
    )
}

/// [`stat_layout`] for the usual four cards.
pub fn stat_layout_default(width: i32) -> (i32, i32) {
    stat_layout(width, 4)
}

/// Height of the meeting notes box: roomy when tall, never below 110.
pub fn meeting_notes_height(viewport_height: i32, top: i32) -> i32 {
    110.max(220.min(viewport_height - top - 330))
}

// ----------------------------------------------------------------- meetings ---

pub const MEETING_SUBTITLE: &str =
    "Transcribe a meeting, jot notes alongside, and save both to your vault.";
/// The title field starts blank; this is drawn, never inserted as text.
pub const MEETING_TITLE_PLACEHOLDER: &str = "Named automatically when saved";
pub const MEETING_NOTES_PLACEHOLDER: &str = "Type notes while you talk: headings and bullets work. When you stop, LocalFlow fills them in from the transcript.";
pub const MEETING_FIELD_LABEL_H: i32 = 18;
pub const MEETING_LABEL_GAP: i32 = 4;
/// `ui_tokens.INPUT_HEIGHT`, shared by field and picker.
pub const MEETING_FIELD_H: i32 = INPUT_HEIGHT;
pub const MEETING_SECTION_GAP: i32 = 14;
pub const MEETING_HEADING_H: i32 = 22;
pub const MEETING_NOTES_STATUS_W: i32 = 220;
/// Notes card -> error (when shown) -> Start button.
pub const MEETING_ACTION_GAP: i32 = 12;

/// Frames of the Meetings form, from the subtitle down to the Start/Stop
/// button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingFormLayout {
    pub subtitle: Frame,
    pub title_label: Frame,
    pub title_field: Frame,
    pub template_label: Frame,
    pub template_field: Frame,
    pub notes_label: Frame,
    pub notes_status: Frame,
    pub notes: Frame,
    pub retry: Frame,
    /// Only present (and only takes room) when there is an error.
    pub error: Option<Frame>,
    /// Top of the Start/Stop button.
    pub action_y: i32,
}

impl MeetingFormLayout {
    /// The named frames that share the page (everything but `retry`, which
    /// replaces the form while the server is down).
    pub fn frames(&self) -> Vec<(&'static str, Frame)> {
        let mut frames = vec![
            ("subtitle", self.subtitle),
            ("title_label", self.title_label),
            ("title_field", self.title_field),
            ("template_label", self.template_label),
            ("template_field", self.template_field),
            ("notes_label", self.notes_label),
            ("notes_status", self.notes_status),
            ("notes", self.notes),
        ];
        if let Some(error) = self.error {
            frames.push(("error", error));
        }
        frames
    }
}

/// Frames of the Meetings form (`meeting_form_layout`).
///
/// The title field and the template picker share one row: the same y and the
/// same height, so they line up exactly. The autosave status sits on the
/// "Your notes" heading row (no space is reserved for it under the card) and
/// an error only takes room when there is one (`error_height > 0`).
pub fn meeting_form_layout(
    width: i32,
    top: i32,
    notes_height: i32,
    error_height: i32,
) -> MeetingFormLayout {
    let title_w = (f64::from(width - 20) * 0.58) as i32;
    let template_x = title_w + 20;
    let form_top = top + 28;
    let field_y = form_top + MEETING_FIELD_LABEL_H + MEETING_LABEL_GAP;
    let heading_y = field_y + MEETING_FIELD_H + MEETING_SECTION_GAP;
    let notes_y = heading_y + MEETING_HEADING_H + MEETING_LABEL_GAP;
    let notes_bottom = notes_y + notes_height;
    let mut y = notes_bottom + MEETING_ACTION_GAP;
    let mut error = None;
    if error_height != 0 {
        error = Some((0, y, width, error_height));
        y += error_height + MEETING_ACTION_GAP;
    }
    MeetingFormLayout {
        subtitle: (0, top, width, 20),
        title_label: (0, form_top, title_w, MEETING_FIELD_LABEL_H),
        title_field: (0, field_y, title_w, MEETING_FIELD_H),
        template_label: (
            template_x,
            form_top,
            width - template_x,
            MEETING_FIELD_LABEL_H,
        ),
        template_field: (template_x, field_y, width - template_x, MEETING_FIELD_H),
        notes_label: (
            0,
            heading_y,
            width - MEETING_NOTES_STATUS_W - 12,
            MEETING_HEADING_H,
        ),
        notes_status: (
            width - MEETING_NOTES_STATUS_W,
            heading_y + 3,
            MEETING_NOTES_STATUS_W,
            18,
        ),
        notes: (0, notes_y, width, notes_height),
        retry: (0, form_top, 100, 36),
        error,
        action_y: y,
    }
}

pub const SYSTEM_AUDIO_PERMISSION: &str =
    "Recording your mic only. Allow system audio to also capture others.";
pub const SYSTEM_AUDIO_UNAVAILABLE: &str =
    "Recording your mic only. System audio isn't available on this Mac right now.";
const PERMISSION_HINTS: [&str; 5] = [
    "permission",
    "not authorized",
    "not permitted",
    "denied",
    "tcc",
];

/// True when a system-audio failure reads as a missing OS permission.
///
/// Only the ScreenCaptureKit path reports these; a missing loopback device,
/// absent bindings or a timeout are not permission problems.
pub fn system_audio_is_permission_error(error: &str) -> bool {
    let text = error.to_lowercase();
    PERMISSION_HINTS.iter().any(|hint| text.contains(hint))
}

/// One plain sentence about what a running meeting is recording. The mic and
/// system-audio failure are reported once; the raw error only decides which
/// sentence is honest. `session` is the server's `session` object.
pub fn meeting_capture_status(session: &Value) -> String {
    match session.get("system_audio").and_then(Value::as_str) {
        Some("on") => "Recording your mic and system audio.".to_string(),
        Some("error") => {
            let error = session
                .get("system_audio_error")
                .and_then(Value::as_str)
                .unwrap_or("");
            if system_audio_is_permission_error(error) {
                SYSTEM_AUDIO_PERMISSION.to_string()
            } else {
                SYSTEM_AUDIO_UNAVAILABLE.to_string()
            }
        }
        _ => "Recording your mic only.".to_string(),
    }
}

/// Python `Path.stem` for a path string.
fn py_stem(path: &str) -> String {
    let separators: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let name = path
        .split(separators)
        .rfind(|part| !part.is_empty() && *part != ".")
        .unwrap_or("");
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// `'Planning'` from `'.../2026-11-15 1430 Planning.md'` (date prefix dropped).
pub fn note_title_from_path(path: &str) -> String {
    let stem = py_stem(path);
    // `_STAMP_PREFIX.sub("", stem)`: `^\d{4}-\d{2}-\d{2} \d{4}\s+`, with
    // Python's Unicode `\d` and `\s`.
    let chars: Vec<char> = stem.chars().collect();
    let stripped: String = match stamp_digits_end(&chars) {
        Some(end) => {
            let spaces = chars[end..].iter().take_while(|c| is_space(**c)).count();
            if spaces > 0 {
                chars[end + spaces..].iter().collect()
            } else {
                stem.clone()
            }
        }
        None => stem.clone(),
    };
    let title = stripped.py_strip();
    if !title.is_empty() {
        title.to_string()
    } else if !stem.is_empty() {
        stem
    } else {
        "Meeting".to_string()
    }
}

/// The saved-state line: the note's title, never its raw path. `saved` is the
/// server's `last_saved` / stop payload.
pub fn saved_notes_message(saved: &Value) -> String {
    let title = saved
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .py_strip()
        .to_string();
    let title = if title.is_empty() {
        note_title_from_path(
            saved
                .get("notes_path")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
    } else {
        title
    };
    format!("Notes saved: {title}")
}

/// The newest saved notes shown on the Meetings page.
pub const RECENT_NOTES_LIMIT: usize = 5;
/// Files looked at, so a huge folder stays cheap.
pub const RECENT_NOTES_SCAN_LIMIT: usize = 2000;
/// `notes_folder/<category>/<file>.md`, with slack.
pub const RECENT_NOTES_MAX_DEPTH: usize = 3;
pub const RECENT_NOTES_UNREADABLE: &str = "Couldn't read your meeting notes folder.";
pub const RECENT_NOTES_OUTSIDE: &str =
    "Your meeting notes folder is outside your vault, so it isn't listed here.";
/// Python reads this many *characters* of the note to find its heading.
const HEAD_CHARS: usize = 4096;

/// Python `re` `\d` is any Unicode decimal digit; `int()` reads them all.
fn digits_at(chars: &[char], start: usize, count: usize) -> Option<u32> {
    let run = chars.get(start..start + count)?;
    if !run.iter().all(|c| is_decimal(*c)) {
        return None;
    }
    parse_decimal(run).map(|value| value as u32)
}

/// End of `\d{4}-\d{2}-\d{2} \d{4}` at the start of `chars`.
fn stamp_digits_end(chars: &[char]) -> Option<usize> {
    let shape = [(4, Some('-')), (2, Some('-')), (2, Some(' ')), (4, None)];
    let mut at = 0;
    for (count, sep) in shape {
        digits_at(chars, at, count)?;
        at += count;
        if let Some(sep) = sep {
            if chars.get(at) != Some(&sep) {
                return None;
            }
            at += 1;
        }
    }
    Some(at)
}

/// A match of Python's `_FILE_STAMP`:
/// `^(\d{4})-(\d{2})-(\d{2}) (\d{2})(\d{2})(?:\s+(.*))?$`.
struct FileStamp {
    /// year, month, day, hour, minute.
    fields: [u32; 5],
    /// Group 6: the text after the stamp (`None` when the group did not take part).
    title: Option<String>,
}

fn file_stamp(stem: &str) -> Option<FileStamp> {
    let chars: Vec<char> = stem.chars().collect();
    let end = stamp_digits_end(&chars)?;
    let fields = [
        digits_at(&chars, 0, 4)?,
        digits_at(&chars, 5, 2)?,
        digits_at(&chars, 8, 2)?,
        digits_at(&chars, 11, 2)?,
        digits_at(&chars, 13, 2)?,
    ];
    let rest = &chars[end..];
    // `$` also matches just before a final "\n".
    if rest.is_empty() || rest == ['\n'] {
        return Some(FileStamp {
            fields,
            title: None,
        });
    }
    // `\s+` is greedy and backtracks: take the longest whitespace prefix for
    // which `(.*)$` (no "\n" except one final) still matches.
    let leading = rest.iter().take_while(|c| is_space(**c)).count();
    for k in (1..=leading).rev() {
        let tail = &rest[k..];
        let body = tail.strip_suffix(&['\n']).unwrap_or(tail);
        if !body.contains(&'\n') {
            return Some(FileStamp {
                fields,
                title: Some(body.iter().collect()),
            });
        }
    }
    None
}

/// Python `str.splitlines()` (without line ends).
fn py_splitlines(text: &str) -> Vec<&str> {
    let is_break = |c: char| {
        matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if is_break(c) {
            lines.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(_, '\n')) = chars.peek() {
                    chars.next();
                    end += 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The note's `# Title` line (after any frontmatter), or `""`.
fn note_heading(path: &Path) -> String {
    let mut raw = Vec::new();
    // At most four bytes per character: enough to decode HEAD_CHARS of them.
    let read = fs::File::open(path)
        .and_then(|file| file.take(HEAD_CHARS as u64 * 4).read_to_end(&mut raw));
    if read.is_err() {
        return String::new();
    }
    let decoded = String::from_utf8_lossy(&raw);
    let head: String = decoded.chars().take(HEAD_CHARS).collect();
    let mut lines = py_splitlines(&head);
    if lines.first().is_some_and(|line| line.py_strip() == "---") {
        if let Some(index) = (1..lines.len()).find(|&i| lines[i].py_strip() == "---") {
            lines = lines[index + 1..].to_vec();
        }
    }
    for line in lines {
        if let Some(rest) = line.strip_prefix("# ") {
            return rest.py_strip().to_string();
        }
        if !line.py_strip().is_empty() {
            return String::new();
        }
    }
    String::new()
}

fn local_from_timestamp(seconds: f64) -> NaiveDateTime {
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9) as u32;
    Local
        .timestamp_opt(whole as i64, nanos.min(999_999_999))
        .single()
        .map_or(chrono::DateTime::UNIX_EPOCH.naive_utc(), |t| {
            t.naive_local()
        })
}

fn note_when(stem: &str, mtime: f64) -> NaiveDateTime {
    if let Some(stamp) = file_stamp(stem) {
        let [y, mo, d, h, mi] = stamp.fields;
        // Python datetime needs 1 <= year <= 9999; `\d` accepts any Unicode
        // decimal digit and `int()` reads it, so those names are dated too.
        if y >= 1 {
            if let Some(when) =
                NaiveDate::from_ymd_opt(y as i32, mo, d).and_then(|date| date.and_hms_opt(h, mi, 0))
            {
                return when;
            }
        }
    }
    local_from_timestamp(mtime)
}

/// `"Today, 3:00 PM"`, `"Nov 14, 2:00 PM"` or `"Nov 14, 2025"` (other year).
/// `now` defaults to the local clock.
pub fn note_date_label(when: NaiveDateTime, now: Option<NaiveDateTime>) -> String {
    let now = now.unwrap_or_else(|| Local::now().naive_local());
    let (pm, hour12) = when.hour12();
    let clock = format!(
        "{hour12}:{:02} {}",
        when.minute(),
        if pm { "PM" } else { "AM" }
    );
    let month = when.format("%b");
    if when.date() == now.date() {
        format!("Today, {clock}")
    } else if when.year() == now.year() {
        format!("{month} {}, {clock}", when.day())
    } else {
        format!("{month} {}, {}", when.day(), when.year())
    }
}

/// One row of the Recent meetings card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentNote {
    pub title: String,
    pub date_label: String,
    pub path: String,
}

/// Python `Path.resolve()` (non-strict): symlinks of the existing prefix are
/// followed, the rest is appended lexically. A symlink loop is an error
/// (Python's `RuntimeError`).
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    let mut links = 0;
    // Owned single-component paths; followed symlink targets are pushed in
    // front so they are resolved before the rest of the original path.
    let mut queue: VecDeque<PathBuf> = absolute
        .components()
        .map(|c| PathBuf::from(c.as_os_str()))
        .collect();
    while let Some(piece) = queue.pop_front() {
        let component = piece.components().next();
        match component {
            None | Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(Component::Prefix(_)) | Some(Component::RootDir) => {
                resolved.push(&piece);
            }
            Some(Component::Normal(name)) => {
                let candidate = resolved.join(name);
                match fs::symlink_metadata(&candidate) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return Err(std::io::Error::other("symlink loop"));
                        }
                        let target = fs::read_link(&candidate)?;
                        let parts: Vec<PathBuf> = target
                            .components()
                            .map(|c| PathBuf::from(c.as_os_str()))
                            .collect();
                        for part in parts.into_iter().rev() {
                            queue.push_front(part);
                        }
                    }
                    _ => resolved = candidate,
                }
            }
        }
    }
    Ok(resolved)
}

/// `os.access(path, R_OK | X_OK)`.
fn readable_dir(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        match std::ffi::CString::new(path.as_os_str().as_bytes()) {
            // SAFETY: `c` is a valid NUL-terminated path for the call.
            Ok(c) => unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::X_OK) == 0 },
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        fs::read_dir(path).is_ok()
    }
}

struct NoteWalk<'a> {
    root: &'a Path,
    found: Vec<(NaiveDateTime, f64, PathBuf)>,
    seen: usize,
    failures: Vec<PathBuf>,
    /// A failure Python would propagate out of the whole listing.
    aborted: bool,
}

impl NoteWalk<'_> {
    /// `os.walk(root, followlinks=False)` plus the filters of
    /// `_recent_notes_in`.
    fn walk(&mut self, dir: &Path, depth: usize) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => {
                self.failures.push(dir.to_path_buf());
                return;
            }
        };
        let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                self.failures.push(dir.to_path_buf());
                return;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            // Like DirEntry.is_dir(): follows symlinks, errors mean "not a dir".
            if fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                let is_link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
                if !is_link {
                    subdirs.push((name, path));
                }
            } else {
                files.push(name);
            }
        }
        if depth >= RECENT_NOTES_MAX_DEPTH {
            subdirs.clear();
        }
        subdirs.retain(|(name, _)| !name.starts_with('.'));
        for name in files {
            if !name.to_lowercase().ends_with(".md") || name.ends_with(" (transcript).md") {
                continue;
            }
            self.seen += 1;
            if self.seen > RECENT_NOTES_SCAN_LIMIT {
                break;
            }
            let path = dir.join(&name);
            let Ok(link_meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if link_meta.file_type().is_symlink() {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            match resolve(&path) {
                Ok(real) if real.starts_with(self.root) => {}
                Ok(_) => continue,
                Err(_) => {
                    self.aborted = true;
                    return;
                }
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let mtime = match modified.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            self.found
                .push((note_when(&py_stem(&name), mtime), mtime, path));
        }
        if self.seen > RECENT_NOTES_SCAN_LIMIT {
            return;
        }
        for (_, sub) in subdirs {
            self.walk(&sub, depth + 1);
            if self.seen > RECENT_NOTES_SCAN_LIMIT || self.aborted {
                return;
            }
        }
    }
}

/// The newest saved meeting notes: `(rows, error)`.
///
/// Same storage the CLI's `lf notes` and recall read: Markdown files under
/// `<vault>/<notes_folder>/` as `ObsidianWriter` writes them
/// (`<category>/YYYY-MM-DD HHMM Title.md`, `# Title` after the frontmatter).
/// Only that folder is walked (never the whole vault), symlinks are not
/// followed, anything resolving outside it is skipped, transcripts are
/// excluded, and only the `limit` newest files are opened for their heading.
/// A missing folder is simply "no notes yet"; an unreadable one is a friendly
/// error, never a panic. `now` is the clock for the date labels (default: the
/// local time).
pub fn recent_notes(
    vault_path: &str,
    notes_folder: &str,
    limit: usize,
    now: Option<NaiveDateTime>,
) -> (Vec<RecentNote>, String) {
    if vault_path.is_empty() || notes_folder.is_empty() {
        return (Vec::new(), String::new());
    }
    let unreadable = || (Vec::new(), RECENT_NOTES_UNREADABLE.to_string());
    let Ok(expanded) = lf_core::home::expand_user(Path::new(vault_path)) else {
        return unreadable();
    };
    let Ok(vault) = resolve(&expanded) else {
        return unreadable();
    };
    let Ok(root) = resolve(&vault.join(notes_folder)) else {
        return unreadable();
    };
    // An absolute or ../ notes_folder, or a notes folder that is itself a
    // symlink out of the vault, is never read.
    if !root.starts_with(&vault) {
        return (Vec::new(), RECENT_NOTES_OUTSIDE.to_string());
    }
    match fs::metadata(&root) {
        Ok(meta) => {
            if !meta.is_dir() || !readable_dir(&root) {
                return unreadable();
            }
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return (Vec::new(), String::new());
        }
        Err(_) => return unreadable(),
    }
    let mut walk = NoteWalk {
        root: &root,
        found: Vec::new(),
        seen: 0,
        failures: Vec::new(),
        aborted: false,
    };
    walk.walk(&root, 0);
    if walk.aborted || (walk.found.is_empty() && walk.failures.iter().any(|f| f == &root)) {
        return unreadable();
    }
    // Newest first; stable like Python's sort(reverse=True).
    walk.found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    let notes = walk
        .found
        .iter()
        .take(limit)
        .map(|(when, _, path)| {
            let stem = py_stem(&path.to_string_lossy());
            let fallback = file_stamp(&stem)
                .and_then(|m| m.title.map(|t| t.py_strip().to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.clone());
            let heading = note_heading(path);
            RecentNote {
                title: if heading.is_empty() {
                    fallback
                } else {
                    heading
                },
                date_label: note_date_label(*when, now),
                path: path.to_string_lossy().into_owned(),
            }
        })
        .collect();
    (notes, String::new())
}

pub const RECENT_ROW_H: i32 = 44;
pub const RECENT_EMPTY: &str = "No saved meeting notes yet. Notes you save appear here.";

// -------------------------------------------------------------------- style ---

pub const STYLE_LABEL_W: i32 = 160;
pub const STYLE_GROUP_X: i32 = 180;
pub const STYLE_ROW_H: i32 = 86;
pub const STYLE_EXAMPLE_Y: i32 = 28;
/// Two 11pt lines: examples wrap instead of truncating.
pub const STYLE_EXAMPLE_H: i32 = 34;
pub const STYLE_SAVE_FAILED: &str =
    "Couldn't save your style settings. Check that ~/.localflow can be written to and try again.";
pub const STYLE_AUTOSAVE_HINT: &str = "Changes save automatically.";

/// Label, wrapped example and segmented group of one style row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleRowFrames {
    pub label: Frame,
    pub example: Frame,
    pub group: Frame,
}

impl StyleRowFrames {
    pub fn all(&self) -> [Frame; 3] {
        [self.label, self.example, self.group]
    }
}

/// The label and its example share the left column (`STYLE_LABEL_W` wide); the
/// example wraps over two lines inside the row, and the group starts right of
/// the column, so nothing overlaps at any width.
pub fn style_row_frames(y: i32, group_width: i32) -> StyleRowFrames {
    StyleRowFrames {
        label: (0, y + 4, STYLE_LABEL_W, 22),
        example: (0, y + STYLE_EXAMPLE_Y, STYLE_LABEL_W, STYLE_EXAMPLE_H),
        group: (STYLE_GROUP_X, y, group_width, 36),
    }
}

/// The config update (for `lf_core::config::apply_config_update`) for the
/// Style page's current selections: `{"cleanup": {"level": .., "style_<name>": ..}}`.
pub fn style_update(cleanup: &str, styles: &[(&str, &str)]) -> Value {
    let mut section = Map::new();
    section.insert("level".into(), cleanup.into());
    for (name, value) in styles {
        section.insert(format!("style_{name}"), (*value).into());
    }
    json!({ "cleanup": Value::Object(section) })
}

// ------------------------------------------------------------------ hotkeys ---

const KEY_NAMES: [(&str, &str); 12] = [
    ("alt_r", "Right Option"),
    ("alt_l", "Left Option"),
    ("alt", "Option"),
    ("ctrl_r", "Right Control"),
    ("ctrl_l", "Left Control"),
    ("ctrl", "Control"),
    ("shift_r", "Right Shift"),
    ("shift_l", "Left Shift"),
    ("shift", "Shift"),
    ("cmd_r", "Right Command"),
    ("cmd_l", "Left Command"),
    ("cmd", "Command"),
];

/// Human label for a config hotkey; an empty value means disabled.
pub fn format_hotkey(value: Option<&str>) -> String {
    let value = value.unwrap_or("").py_strip();
    if value.is_empty() {
        return "Off".to_string();
    }
    value
        .split('+')
        .map(|part| {
            let part = part.py_strip().trim_matches(['<', '>']).to_lowercase();
            if let Some((_, name)) = KEY_NAMES.iter().find(|(key, _)| *key == part) {
                (*name).to_string()
            } else if part.chars().count() == 1 {
                part.to_uppercase()
            } else {
                py_title(&part)
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// One row of the Prompts page's shortcut card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    pub name: &'static str,
    pub detail: &'static str,
    pub hotkey: String,
}

/// The dictation shortcuts, read from the config's current hotkeys. None of
/// them has an enable flag of its own: an empty hotkey disables it.
pub fn shortcut_rows(config: &Config) -> Vec<ShortcutRow> {
    let hotkey = |field: &str| format_hotkey(config.field_str(field));
    vec![
        ShortcutRow {
            name: "Prompt Engineer",
            detail: "Select rough text, press the shortcut, get an AI-ready prompt.",
            hotkey: hotkey("prompt_hotkey"),
        },
        ShortcutRow {
            name: "Polish",
            detail: "Select text and clean up its wording and grammar in place.",
            hotkey: hotkey("polish_hotkey"),
        },
        ShortcutRow {
            name: "Command Mode",
            detail: "Hold the key and speak an instruction to rewrite your selection.",
            hotkey: hotkey("command_hotkey"),
        },
        ShortcutRow {
            name: "Toggle Dictation",
            detail: "Turn push-to-talk on or off without quitting LocalFlow.",
            hotkey: hotkey("dictation_toggle_hotkey"),
        },
    ]
}

// --------------------------------------------------------------- dictionary ---

pub const DICTIONARY_SAVE_FAILED: &str =
    "Couldn't save to your dictionary. Check that ~/.localflow can be written to and try again.";

/// The dictionary operations the Hub performs. Implemented for
/// [`DictionaryStore`]; tests inject failing stores.
pub trait DictionaryEditor {
    fn add_word(&self, word: &str) -> anyhow::Result<bool>;
    fn update_word(&self, old: &str, new: &str) -> anyhow::Result<bool>;
    fn add_replacement(&self, source: &str, target: &str) -> anyhow::Result<bool>;
    fn update_replacement(
        &self,
        old_from: &str,
        new_from: &str,
        new_to: &str,
    ) -> anyhow::Result<bool>;
    fn remove(&self, value: &str, kind: Option<&str>) -> anyhow::Result<bool>;
}

impl DictionaryEditor for DictionaryStore {
    fn add_word(&self, word: &str) -> anyhow::Result<bool> {
        DictionaryStore::add_word(self, word)
    }
    fn update_word(&self, old: &str, new: &str) -> anyhow::Result<bool> {
        DictionaryStore::update_word(self, old, new)
    }
    fn add_replacement(&self, source: &str, target: &str) -> anyhow::Result<bool> {
        DictionaryStore::add_replacement(self, source, target)
    }
    fn update_replacement(
        &self,
        old_from: &str,
        new_from: &str,
        new_to: &str,
    ) -> anyhow::Result<bool> {
        DictionaryStore::update_replacement(self, old_from, new_from, new_to)
    }
    fn remove(&self, value: &str, kind: Option<&str>) -> anyhow::Result<bool> {
        DictionaryStore::remove(self, value, kind)
    }
}

/// What a dictionary list row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Word,
    Replacement,
}

impl EntryKind {
    /// The store's `kind` argument (`"word"` / `"replacement"`).
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Word => "word",
            EntryKind::Replacement => "replacement",
        }
    }
}

/// `(label, key, kind, target)` of a dictionary row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictionaryRow {
    pub label: String,
    pub key: String,
    pub kind: EntryKind,
    pub target: String,
}

/// A row for every word and replacement; nothing capped.
pub fn dictionary_entries(data: &DictionaryData) -> Vec<DictionaryRow> {
    let mut entries: Vec<DictionaryRow> = data
        .words
        .iter()
        .map(|word| DictionaryRow {
            label: word.clone(),
            key: word.clone(),
            kind: EntryKind::Word,
            target: String::new(),
        })
        .collect();
    entries.extend(data.replacements.iter().map(|pair| DictionaryRow {
        label: format!("{} → {}", pair.from, pair.to),
        key: pair.from.clone(),
        kind: EntryKind::Replacement,
        target: pair.to.clone(),
    }));
    entries
}

fn is_dictionary_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DictionaryError>().is_some()
}

/// Add (or, when `editing`, rename) a word. Returns a friendly error or `None`.
pub fn submit_word(
    store: &dyn DictionaryEditor,
    text: &str,
    editing: Option<&str>,
) -> Option<String> {
    if text.py_strip().is_empty() {
        return Some("Type a word first.".to_string());
    }
    let outcome = match editing.filter(|e| !e.is_empty()) {
        Some(old) => store.update_word(old, text).map(|ok| {
            (!ok).then(|| "That word was removed or is already in your dictionary.".to_string())
        }),
        None => store.add_word(text).map(|ok| {
            (!ok).then(|| format!("“{}” is already in your dictionary.", text.py_strip()))
        }),
    };
    match outcome {
        Ok(message) => message,
        Err(error) if is_dictionary_error(&error) => Some("Type a word first.".to_string()),
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

/// Add (or edit) an auto-replace pair. Returns a friendly error or `None`.
pub fn submit_replacement(
    store: &dyn DictionaryEditor,
    source: &str,
    target: &str,
    editing: Option<&str>,
) -> Option<String> {
    if source.py_strip().is_empty() {
        return Some("Type what you say first.".to_string());
    }
    if target.py_strip().is_empty() {
        return Some("Type what it should become.".to_string());
    }
    let outcome = match editing.filter(|e| !e.is_empty()) {
        Some(old) => store
            .update_replacement(old, source, target)
            .map(|ok| (!ok).then(|| "That replacement was removed or already exists.".to_string())),
        None => store.add_replacement(source, target).map(|ok| {
            (!ok).then(|| {
                format!(
                    "You already have a replacement for “{}”.",
                    source.py_strip()
                )
            })
        }),
    };
    match outcome {
        Ok(message) => message,
        Err(error) if is_dictionary_error(&error) => {
            Some("Fill in both “When I say” and “Type this”.".to_string())
        }
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

/// Remove one dictionary entry (of `kind`). Returns a friendly error or `None`.
pub fn remove_entry(
    store: &dyn DictionaryEditor,
    key: &str,
    kind: Option<EntryKind>,
) -> Option<String> {
    match store.remove(key, kind.map(EntryKind::as_str)) {
        Ok(_) => None,
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

// ------------------------------------------------------------------ prompts ---

pub const PROMPTS_EMPTY: &str = "Prompts generated from meetings show up here for approval.";
pub const PROMPTS_UNREADABLE: &str =
    "Couldn't read your prompt queue. Check ~/.localflow and try again.";
/// Status pill: `(status, (tint token, text token))`.
pub const PROMPT_STATUS_PILLS: [(&str, (&str, &str)); 5] = [
    ("pending", ("sun", "ink")),
    ("approved", ("success", "success")),
    ("rejected", ("line", "muted")),
    ("dispatched", ("cobalt", "cobalt")),
    ("failed", ("red", "red")),
];

/// `PROMPT_STATUS_PILLS[status]`.
pub fn prompt_status_pill(status: &str) -> Option<(&'static str, &'static str)> {
    PROMPT_STATUS_PILLS
        .iter()
        .find(|(name, _)| *name == status)
        .map(|(_, pill)| *pill)
}

/// Where a prompt came from. The queue stores `repo` and `meeting`, not a
/// project.
pub fn prompt_context(entry: &PromptEntry) -> String {
    text_field(entry, "repo")
        .or_else(|| text_field(entry, "meeting"))
        .unwrap_or_else(|| "Unknown meeting".to_string())
}

pub const ORCA_NOT_OPEN: &str = "Couldn't send — the Orca agent app isn't open";

/// The failure shown on a prompt row: shared copy for a known cause only.
///
/// "Orca app is not running — run `orca open`…" (what `orca_available`
/// records) and the older "Orca is not running" map to [`ORCA_NOT_OPEN`];
/// other failures (a missing CLI, an unreachable runtime, a bad repo, a
/// timeout) keep their own text, whitespace collapsed.
pub fn prompt_error_text(error: &str) -> String {
    use std::sync::OnceLock;
    static NOT_RUNNING: OnceLock<Regex> = OnceLock::new();
    let re = NOT_RUNNING
        .get_or_init(|| Regex::new(r"(?i)\borca(?: app)? is not running\b").expect("static regex"));
    let text = error.py_split().join(" ");
    if re.is_match(&text) {
        ORCA_NOT_OPEN.to_string()
    } else {
        text
    }
}

/// One row of the Prompts page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRow {
    pub id: String,
    pub title: String,
    pub context: String,
    pub error: String,
    pub status: String,
    pub status_label: String,
    pub can_approve: bool,
    pub can_reject: bool,
}

/// Newest first. Buttons follow the same guards the server API enforces.
pub fn prompt_rows(entries: &[PromptEntry]) -> Vec<PromptRow> {
    entries
        .iter()
        .rev()
        .map(|entry| {
            let status = text_field(entry, "status").unwrap_or_else(|| "pending".to_string());
            let mut context = prompt_context(entry);
            let error = if status == "failed" {
                prompt_error_text(&text_field(entry, "error").unwrap_or_default())
            } else {
                String::new()
            };
            if !error.is_empty() {
                context = format!("{context} · {error}");
            }
            PromptRow {
                id: text_field(entry, "id").unwrap_or_default(),
                title: text_field(entry, "title").unwrap_or_else(|| "Untitled prompt".to_string()),
                context,
                error,
                status_label: py_title(&status),
                can_approve: APPROVE_FROM.contains(&status.as_str()),
                can_reject: REJECT_FROM.contains(&status.as_str()),
                status,
            }
        })
        .collect()
}

pub const PROMPT_ROW_MIN_H: i32 = 68;
pub const PROMPT_TITLE_Y: i32 = 12;
pub const PROMPT_CONTEXT_Y: i32 = 36;
pub const PROMPT_CONTEXT_LINE: i32 = 16;
pub const PROMPT_ROW_BOTTOM: i32 = 14;
pub const PROMPT_BUTTON_APPROVE: i32 = 88;
pub const PROMPT_BUTTON_REJECT: i32 = 80;
pub const PROMPT_BUTTON_GAP: i32 = 8;
pub const PROMPT_EDGE: i32 = 12;
/// Font of the prompt context line: 11pt regular.
pub const PROMPT_CONTEXT_FONT_SIZE: f64 = 11.0;

/// Geometry of one prompt row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRowLayout {
    pub action_x: i32,
    pub pill_x: i32,
    pub text_width: i32,
    pub context_h: i32,
    pub height: i32,
    pub lines: usize,
}

/// Geometry of one prompt row: buttons, pill, text column and height.
///
/// The context line (source plus any failure) wraps inside the text column
/// and the row grows to fit it, so a friendly failure sentence is never
/// truncated even at the minimum window width with both actions shown.
/// `measure` is the real 11pt measurement in AppKit; without it a
/// conservative `6.5 pt per character` estimate is used.
pub fn prompt_row_layout(
    width: i32,
    row: &PromptRow,
    pill_width: i32,
    measure: Option<TextMeasure<'_>>,
) -> PromptRowLayout {
    let mut widths = Vec::new();
    if row.can_approve {
        widths.push(PROMPT_BUTTON_APPROVE);
    }
    if row.can_reject {
        widths.push(PROMPT_BUTTON_REJECT);
    }
    let gaps = PROMPT_BUTTON_GAP * (widths.len() as i32 - 1).max(0);
    let action_x = width - PROMPT_EDGE - widths.iter().sum::<i32>() - gaps;
    let pill_x = action_x - pill_width - 12;
    let text_width = (pill_x - 12 - 14).max(40);
    let estimate = |text: &str, _size: f64, _weight: Weight| text.chars().count() as f64 * 6.5;
    let measure: TextMeasure<'_> = measure.unwrap_or(&estimate);
    let lines = wrap_lines(&row.context, f64::from(text_width), &|text: &str| {
        measure(text, PROMPT_CONTEXT_FONT_SIZE, Weight::Regular)
    });
    let context_h = (lines.len() as i32).max(1) * PROMPT_CONTEXT_LINE + 2;
    let height = PROMPT_ROW_MIN_H.max(PROMPT_CONTEXT_Y + context_h + PROMPT_ROW_BOTTOM);
    PromptRowLayout {
        action_x,
        pill_x,
        text_width,
        context_h,
        height,
        lines: lines.len(),
    }
}

/// Approve or reject (the two buttons of a prompt row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    Approve,
    Reject,
}

/// Approve or reject through the same guarded helpers the server uses.
///
/// Returns a friendly, actionable message when it could not be done.
pub fn review_prompt(
    queue: &dyn PromptQueue,
    entry_id: &str,
    action: ReviewAction,
) -> Option<String> {
    let outcome = match action {
        ReviewAction::Approve => approve_prompt(queue, entry_id),
        ReviewAction::Reject => reject_prompt(queue, entry_id),
    };
    match outcome {
        Ok(_) => None,
        Err(TransitionFailure::Refused(refused)) => {
            if refused.status_code == 404 {
                return Some(
                    "That prompt is no longer in the queue. The list was refreshed.".into(),
                );
            }
            let past = match action {
                ReviewAction::Approve => "approved",
                ReviewAction::Reject => "rejected",
            };
            Some(format!(
                "That prompt changed before it could be {past}, so nothing was changed. The list was refreshed."
            ))
        }
        Err(TransitionFailure::Queue(_)) => {
            Some("Couldn't update the prompt queue. Check ~/.localflow and try again.".into())
        }
    }
}

/// Queue entries for the Prompts page, or the friendly error when the queue is
/// unreadable (`_prompt_entries`).
pub fn prompt_entries(queue: &dyn PromptQueue) -> Result<Vec<PromptEntry>, &'static str> {
    queue.list(None).map_err(|_| PROMPTS_UNREADABLE)
}

// ===================================================== Home / History pages ===
// Pure helpers of the Home and History pages (lf-ui `mac::pages::{home,history}`).

/// The Home page's dictation switch state (`hub_model.dictation_state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationState {
    pub enabled: bool,
    pub label: String,
    pub action: &'static str,
    pub detail: &'static str,
}

/// `dictation_state(config)`.
pub fn dictation_state(config: &Config) -> DictationState {
    let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
    DictationState {
        enabled,
        label: format!("Dictation: {}", if enabled { "On" } else { "Off" }),
        action: if enabled {
            "Turn Dictation Off"
        } else {
            "Turn Dictation On"
        },
        detail: if enabled {
            "Dictation is on — push-to-talk is ready"
        } else {
            "Dictation is off — push-to-talk is paused"
        },
    }
}

/// Rows per History page (`_render_history` shows 8 and probes a 9th).
pub const HISTORY_PAGE_SIZE: i64 = 8;
/// Height of one History/Home row.
pub const HISTORY_ROW_HEIGHT: i32 = 56;
/// Python's `(text, size)` fallback when an entry has no text.
pub const HISTORY_NO_TEXT: &str = "(no text)";

/// `final_text or raw_text or "(no text)"`.
pub fn history_row_text(final_text: &str, raw_text: &str) -> String {
    if !final_text.is_empty() {
        final_text.to_string()
    } else if !raw_text.is_empty() {
        raw_text.to_string()
    } else {
        HISTORY_NO_TEXT.to_string()
    }
}

/// `"3:05 PM"` for today, `"Nov 5"` otherwise (`_history_row`).
pub fn history_date_label(when: NaiveDateTime, now: NaiveDateTime) -> String {
    if when.date() == now.date() {
        let (pm, hour12) = when.hour12();
        format!(
            "{hour12}:{:02} {}",
            when.minute(),
            if pm { "PM" } else { "AM" }
        )
    } else {
        // `strftime("%b %d").replace(" 0", " ")`
        format!("{} {}", when.format("%b"), when.day())
    }
}

/// Local date-time of a history timestamp (seconds since the epoch).
pub fn history_when(ts: f64) -> NaiveDateTime {
    local_from_timestamp(ts)
}

/// `"{app or Unknown}  ·  {date}  ·  {Kind}"` (`kind.title()`).
pub fn history_meta_line(
    app_name: Option<&str>,
    ts: f64,
    kind: &str,
    now: NaiveDateTime,
) -> String {
    let app = app_name.filter(|a| !a.is_empty()).unwrap_or("Unknown");
    let mut title = String::new();
    let mut start = true;
    for c in kind.chars() {
        if c.is_alphabetic() {
            if start {
                title.extend(c.to_uppercase());
            } else {
                title.extend(c.to_lowercase());
            }
            start = false;
        } else {
            title.push(c);
            start = true;
        }
    }
    format!(
        "{app}  ·  {}  ·  {title}",
        history_date_label(history_when(ts), now)
    )
}

/// The status chip of a non-`ok` entry: `(label, failed)`.
pub fn history_status_chip(status: &str) -> Option<(&'static str, bool)> {
    match status {
        "ok" => None,
        "failed" => Some(("Failed", true)),
        _ => Some(("Silent", false)),
    }
}

/// `(title, width)` of the per-row action buttons, in order (Copy, Original,
/// Retry for dictations that kept their audio, Delete).
pub fn history_row_actions(kind: &str, has_audio: bool) -> Vec<(&'static str, i32)> {
    let mut entries = vec![("Copy", 56), ("Original", 76)];
    if kind == "dictate" && has_audio {
        entries.push(("Retry", 60));
    }
    entries.push(("Delete", 72));
    entries
}

/// x where the action buttons start in a row `width` wide (`width` when the
/// row has no actions).
pub fn history_action_start(width: i32, actions: &[(&'static str, i32)]) -> i32 {
    if actions.is_empty() {
        return width;
    }
    width - 12 - actions.iter().map(|(_, w)| *w).sum::<i32>() - 8 * (actions.len() as i32 - 1)
}

/// Geometry of one row's text and chip: `(text_width, chip_x)`; `chip_x` is
/// `None` for an `ok` entry.
pub fn history_row_text_layout(
    width: i32,
    action_start: Option<i32>,
    failed_or_silent: bool,
) -> (i32, Option<i32>) {
    let mut text_width = match action_start {
        Some(start) => start - 24,
        None => width - 40,
    };
    if !failed_or_silent {
        return (text_width, None);
    }
    let chip_x = match action_start {
        Some(start) => start - 82,
        None => width - 102,
    };
    text_width = chip_x - 26;
    (text_width, Some(chip_x))
}

/// The four Home stat cards `(title, value)` (`_page_home`).
pub fn home_stat_cards(stats: &lf_core::history::Stats) -> [(&'static str, String); 4] {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    [
        ("Words", stats.total_words.to_string()),
        ("Avg WPM", format!("{:.1}", stats.avg_wpm)),
        (
            "Streak",
            format!(
                "{} day{}",
                stats.current_streak_days,
                plural(stats.current_streak_days)
            ),
        ),
        ("Dictations", stats.total_dictations.to_string()),
    ]
}

/// The "Today … Last 7 days … Longest streak …" line under the cards.
pub fn home_stats_summary(stats: &lf_core::history::Stats) -> String {
    format!(
        "Today  {} words     Last 7 days  {} words     Longest streak  {} day{}",
        stats.words_today,
        stats.words_7d,
        stats.longest_streak_days,
        if stats.longest_streak_days == 1 {
            ""
        } else {
            "s"
        }
    )
}

/// `(title, detail, button, action id)` of an empty list. `query` is the
/// active History search (`None` on Home, which says "Your recent dictations").
pub fn history_empty_state(
    searching: bool,
    home: bool,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if searching {
        (
            "No matching entries",
            "Try another search or start a new dictation.",
            "Clear search",
            "clearSearch",
        )
    } else if home {
        (
            "No dictations yet",
            "Your recent dictations will appear here.",
            "Open setup",
            "openSetup",
        )
    } else {
        (
            "No dictations yet",
            "Your dictations will appear here.",
            "Open setup",
            "openSetup",
        )
    }
}

/// `"Page N"` for the History pager.
pub fn history_page_label(offset: i64) -> String {
    format!("Page {}", offset / HISTORY_PAGE_SIZE + 1)
}

/// Offset after "Newer".
pub fn history_newer_offset(offset: i64) -> i64 {
    (offset - HISTORY_PAGE_SIZE).max(0)
}

/// Offset after "Older".
pub fn history_older_offset(offset: i64) -> i64 {
    offset + HISTORY_PAGE_SIZE
}

/// Text copied by Copy (`final_text`) / Original (`raw_text`).
pub const HISTORY_CLEAR_CONFIRM: &str = "Clear all history and stored clips?";
/// Shown when Retry is pressed: re-transcription needs speech-to-text, which
/// this Rust build does not contain.
pub const HISTORY_RETRY_UNAVAILABLE: &str =
    "Retry needs speech-to-text, which isn't available in this build.";

#[cfg(test)]
mod home_history_tests {
    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn dictation_state_tracks_enabled_config() {
        let mut config = Config::defaults_for(true);
        let state = dictation_state(&config);
        assert!(state.enabled);
        assert_eq!(state.label, "Dictation: On");
        assert_eq!(state.action, "Turn Dictation Off");
        assert_eq!(state.detail, "Dictation is on — push-to-talk is ready");
        config.set_field("dictation_enabled", serde_json::Value::Bool(false));
        let state = dictation_state(&config);
        assert!(!state.enabled);
        assert_eq!(state.label, "Dictation: Off");
        assert_eq!(state.action, "Turn Dictation On");
        assert_eq!(state.detail, "Dictation is off — push-to-talk is paused");
    }

    #[test]
    fn row_text_and_dates_follow_python() {
        assert_eq!(history_row_text("final", "raw"), "final");
        assert_eq!(history_row_text("", "raw"), "raw");
        assert_eq!(history_row_text("", ""), "(no text)");
        let now = dt(2026, 11, 16, 20, 0);
        assert_eq!(history_date_label(dt(2026, 11, 16, 15, 5), now), "3:05 PM");
        assert_eq!(history_date_label(dt(2026, 11, 16, 0, 7), now), "12:07 AM");
        assert_eq!(history_date_label(dt(2026, 11, 5, 9, 0), now), "Nov 5");
        assert_eq!(history_date_label(dt(2026, 11, 15, 9, 0), now), "Nov 15");
    }

    #[test]
    fn row_actions_and_layout() {
        let a = history_row_actions("dictate", true);
        assert_eq!(
            a.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Copy", "Original", "Retry", "Delete"]
        );
        assert_eq!(history_row_actions("dictate", false).len(), 3);
        assert_eq!(history_row_actions("note", true).len(), 3);
        // 1000 - 12 - (56+76+60+72) - 8*3
        assert_eq!(history_action_start(1000, &a), 1000 - 12 - 264 - 24);
        assert_eq!(history_action_start(1000, &[]), 1000);
        assert_eq!(history_row_text_layout(1000, None, false), (960, None));
        assert_eq!(
            history_row_text_layout(1000, None, true),
            (1000 - 102 - 26, Some(898))
        );
        assert_eq!(history_row_text_layout(1000, Some(600), false), (576, None));
        assert_eq!(
            history_row_text_layout(1000, Some(600), true),
            (492, Some(518))
        );
    }

    #[test]
    fn chips_and_titles() {
        assert_eq!(history_status_chip("ok"), None);
        assert_eq!(history_status_chip("failed"), Some(("Failed", true)));
        assert_eq!(history_status_chip("silent"), Some(("Silent", false)));
        let now = dt(2026, 11, 16, 20, 0);
        let when = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let ts = Local
            .from_local_datetime(&when.and_hms_opt(9, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp() as f64;
        assert_eq!(
            history_meta_line(None, ts, "dictate", now),
            "Unknown  ·  Nov 5  ·  Dictate"
        );
        assert_eq!(
            history_meta_line(Some("Mail"), ts, "MEETING note", now),
            "Mail  ·  Nov 5  ·  Meeting Note"
        );
    }

    #[test]
    fn paging_and_empty_states() {
        assert_eq!(history_page_label(0), "Page 1");
        assert_eq!(history_page_label(16), "Page 3");
        assert_eq!(history_newer_offset(4), 0);
        assert_eq!(history_newer_offset(16), 8);
        assert_eq!(history_older_offset(8), 16);
        assert_eq!(history_empty_state(true, false).3, "clearSearch");
        assert_eq!(
            history_empty_state(false, true).1,
            "Your recent dictations will appear here."
        );
        assert_eq!(
            history_empty_state(false, false).1,
            "Your dictations will appear here."
        );
    }

    #[test]
    fn stat_cards_pluralise() {
        let stats = lf_core::history::Stats {
            total_words: 1234,
            total_dictations: 9,
            avg_wpm: 101.25,
            words_today: 3,
            words_7d: 40,
            current_streak_days: 1,
            longest_streak_days: 5,
            top_apps: vec![],
        };
        let cards = home_stat_cards(&stats);
        assert_eq!(cards[1], ("Avg WPM", "101.2".to_string()));
        assert_eq!(cards[2], ("Streak", "1 day".to_string()));
        assert_eq!(
            home_stats_summary(&stats),
            "Today  3 words     Last 7 days  40 words     Longest streak  5 days"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::prompts::{FakePromptQueue, WorkPrompt};
    use crate::tokens::{HUB_CONTENT_X, HUB_MIN_HEIGHT, HUB_MIN_WIDTH};
    use std::time::{Duration, SystemTime};

    const WIDTHS: [i32; 5] = [
        HUB_MIN_WIDTH - HUB_CONTENT_X - 32,
        640,
        760,
        1040 - HUB_CONTENT_X - 32,
        1400 - HUB_CONTENT_X - 32,
    ];

    fn overlaps(a: Frame, b: Frame) -> bool {
        a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
    }

    fn entry(value: Value) -> PromptEntry {
        value.as_object().unwrap().clone()
    }

    // ---- sidebar / home -------------------------------------------------

    #[test]
    fn models_sidebar_label_is_display_only_and_fits() {
        assert_eq!(sidebar_label("Models"), "Models & keys");
        assert_eq!(sidebar_label("Home"), "Home");
        let measure = |text: &str, size: f64, weight: Weight| {
            assert_eq!((size, weight), (13.0, Weight::Semibold));
            text.chars().count() as f64 * 8.0
        };
        let title = sidebar_title("Models", Some(&measure), SIDEBAR_BUTTON_WIDTH);
        assert_eq!(title.py_strip(), "Models & keys");
        assert!(
            measure(&title, 13.0, Weight::Semibold)
                <= f64::from(SIDEBAR_BUTTON_WIDTH - SIDEBAR_TITLE_INSET)
        );
        for page in crate::hub_route::HUB_PAGES {
            let title = sidebar_title(page, Some(&measure), SIDEBAR_BUTTON_WIDTH);
            assert!(measure(&title, 13.0, Weight::Semibold) <= 152.0, "{page}");
        }
        // A label that would not fit falls back to the page id, never truncates.
        let huge = |_: &str, _: f64, _: Weight| 999.0;
        assert_eq!(
            sidebar_title("Models", Some(&huge), SIDEBAR_BUTTON_WIDTH).py_strip(),
            "Models"
        );
        // Without a measure the label is used as is.
        assert_eq!(
            sidebar_title("Models", None, 1),
            "\u{2002}\u{2002}Models & keys"
        );
    }

    /// `hub_model.SIDEBAR_TITLE_PREFIX` is two U+2002 EN SPACE characters, not
    /// ASCII spaces; the codepoints are spelled out so a normalising edit fails.
    #[test]
    fn sidebar_title_prefix_is_two_en_spaces() {
        assert_eq!(
            SIDEBAR_TITLE_PREFIX
                .chars()
                .map(|c| c as u32)
                .collect::<Vec<_>>(),
            vec![0x2002, 0x2002]
        );
        assert_eq!(SIDEBAR_TITLE_PREFIX.len(), 6);
        assert!(!SIDEBAR_TITLE_PREFIX.contains(' '));
        let huge = |_: &str, _: f64, _: Weight| 999.0;
        let fallback = sidebar_title("Models", Some(&huge), SIDEBAR_BUTTON_WIDTH);
        assert_eq!(
            fallback
                .chars()
                .take(2)
                .map(|c| c as u32)
                .collect::<Vec<_>>(),
            vec![0x2002, 0x2002]
        );
        assert_eq!(fallback, "\u{2002}\u{2002}Models");
    }

    #[test]
    fn note_stamps_accept_unicode_decimal_digits_like_python() {
        // Arabic-Indic digits (Nd): Python's `\d` matches and `int()` reads them.
        let stem = "\u{661}\u{662}\u{663}\u{664}-\u{660}\u{661}-\u{660}\u{662} \u{661}\u{662}\u{663}\u{660} Plan";
        // 1234-01-02 12:30
        let expected = NaiveDate::from_ymd_opt(1234, 1, 2)
            .unwrap()
            .and_hms_opt(12, 30, 0)
            .unwrap();
        assert_eq!(note_when(stem, 0.0), expected);
        assert_eq!(note_title_from_path(&format!("/v/{stem}.md")), "Plan");
        let stamp = file_stamp(stem).unwrap();
        assert_eq!(stamp.fields, [1234, 1, 2, 12, 30]);
        assert_eq!(stamp.title.as_deref(), Some("Plan"));
        // Fullwidth digits too.
        let wide = "２０２６-１１-１５ １４３０ Ａ";
        assert_eq!(note_title_from_path(wide), "Ａ");
        assert_eq!(file_stamp(wide).unwrap().fields, [2026, 11, 15, 14, 30]);
    }

    #[test]
    fn note_stamp_edge_cases_follow_the_python_regex() {
        // Information separators are `\s` in Python.
        let sep = "2026-11-15 1430\u{1c}Plan";
        assert_eq!(file_stamp(sep).unwrap().title.as_deref(), Some("Plan"));
        assert_eq!(note_title_from_path(&format!("{sep}.md")), "Plan");
        // No trailing text: group 6 does not take part.
        assert!(file_stamp("2026-11-15 1430").unwrap().title.is_none());
        // `$` matches before a final newline; `.` never matches a newline.
        assert_eq!(
            file_stamp("2026-11-15 1430 A\n").unwrap().title.as_deref(),
            Some("A")
        );
        assert!(file_stamp("2026-11-15 1430 A\nB").is_none());
        // Text glued to the time is not a stamp; neither are short fields.
        assert!(file_stamp("2026-11-15 1430Plan").is_none());
        assert!(file_stamp("2026-1-15 1430 Plan").is_none());
        // Impossible dates and year 0 fall back to the modification time.
        let fallback = local_from_timestamp(86_400.0 * 400.0);
        assert_eq!(note_when("2026-13-15 1430 X", 86_400.0 * 400.0), fallback);
        assert_eq!(note_when("0000-01-01 1200 X", 86_400.0 * 400.0), fallback);
        assert_eq!(note_when("2026-11-15 2460 X", 86_400.0 * 400.0), fallback);
        assert_eq!(
            note_title_from_path("/v/2026-11-15 1430 .md"),
            "2026-11-15 1430 "
        );
    }

    #[test]
    fn stat_cards_wrap_and_notes_box_fits_the_minimum() {
        let body_min = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * 16;
        let (columns, card) = stat_layout_default(body_min);
        assert_eq!(columns, 2);
        assert!(card * columns + 12 <= body_min);
        let (columns, card) = stat_layout_default(1040 - HUB_CONTENT_X - 32);
        assert_eq!(columns, 4);
        assert!(card >= STAT_FOUR_COLUMN_MIN_CARD);
        let height = meeting_notes_height(HUB_MIN_HEIGHT - 24, 48);
        assert!((110..=220).contains(&height));
        assert_eq!(meeting_notes_height(2000, 48), 220);
        assert_eq!(meeting_notes_height(100, 48), 110);
        // Degenerate widths still give one column and a non-negative card.
        assert_eq!(stat_layout(100, 4), (1, 100));
        assert_eq!(stat_layout(0, 4).0, 1);
        assert_eq!(stat_layout(500, 3), (3, 158));
        assert_eq!(stat_layout(400, 3), (1, 400));
    }

    #[test]
    fn home_decoration_stays_in_the_header_band() {
        for width in WIDTHS {
            let (x, y, w, h) = home_decoration_frame(width);
            assert!(y >= 0 && y + h <= HUB_HEADER_OFFSET);
            assert!(x >= 0 && x + w <= width);
            let (columns, card_width) = stat_layout_default(width);
            for column in 0..columns {
                let card = (
                    column * (card_width + 12),
                    HUB_HEADER_OFFSET,
                    card_width,
                    104,
                );
                assert!(!overlaps((x, y, w, h), card), "{width}");
            }
            let dots =
                crate::tokens::halftone_dots(f64::from(w), f64::from(h), 9, 3, 4, "top-right");
            assert!(!dots.is_empty());
            for (dx, dy, d) in dots {
                assert!(dx >= 0.0 && dx + d <= f64::from(w) && dy >= 0.0 && dy + d <= f64::from(h));
            }
        }
    }

    // ---- meetings ---------------------------------------------------------

    #[test]
    fn meeting_form_layout_has_no_overlaps_and_aligned_inputs() {
        for width in WIDTHS {
            for error_height in [0, 34] {
                let top = HUB_HEADER_OFFSET;
                let notes = meeting_notes_height(HUB_MIN_HEIGHT - 24, top);
                let layout = meeting_form_layout(width, top, notes, error_height);
                let (title, template) = (layout.title_field, layout.template_field);
                assert_eq!(title.1, template.1);
                assert_eq!(title.3, INPUT_HEIGHT);
                assert_eq!(template.3, INPUT_HEIGHT);
                assert_eq!(layout.title_label.1, layout.template_label.1);
                assert!(title.0 + title.2 < template.0);
                assert_eq!(template.0 + template.2, width);
                let mut boxes = layout.frames();
                boxes.push(("action", (0, layout.action_y, 250, 36)));
                for (i, (a, fa)) in boxes.iter().enumerate() {
                    for (b, fb) in &boxes[i + 1..] {
                        assert!(!overlaps(*fa, *fb), "{a} {b} at {width}");
                    }
                    assert!(fa.0 >= 0 && fa.0 + fa.2 <= width, "{a}");
                }
                assert_eq!(layout.subtitle.1, top);
                let notes_bottom = layout.notes.1 + layout.notes.3;
                if error_height > 0 {
                    let error = layout.error.unwrap();
                    assert_eq!(error.1, notes_bottom + 12);
                    assert_eq!(layout.action_y, error.1 + error_height + 12);
                } else {
                    // No reserved blank band: Start sits 12pt under the card.
                    assert!(layout.error.is_none());
                    assert_eq!(layout.action_y, notes_bottom + 12);
                }
                assert!(layout.notes_status.1 < layout.notes.1);
            }
        }
    }

    #[test]
    fn capture_status_is_one_plain_honest_sentence() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"system_audio": "on"}),
                "Recording your mic and system audio.",
            ),
            (json!({"system_audio": "off"}), "Recording your mic only."),
            (json!({}), "Recording your mic only."),
            (Value::Null, "Recording your mic only."),
            (
                json!({"system_audio": "error", "system_audio_error": "ScreenCaptureKit: no display available — grant Screen Recording permission to this Python binary (None)"}),
                SYSTEM_AUDIO_PERMISSION,
            ),
            (
                json!({"system_audio": "error", "system_audio_error": "System audio permission unavailable"}),
                SYSTEM_AUDIO_PERMISSION,
            ),
            (
                json!({"system_audio": "error", "system_audio_error": "no system audio source found: set [meetings] system_audio_device"}),
                SYSTEM_AUDIO_UNAVAILABLE,
            ),
            (
                json!({"system_audio": "error", "system_audio_error": "ScreenCaptureKit: start timed out"}),
                SYSTEM_AUDIO_UNAVAILABLE,
            ),
            (
                json!({"system_audio": "error", "system_audio_error": "no input device matching 'X'"}),
                SYSTEM_AUDIO_UNAVAILABLE,
            ),
            (json!({"system_audio": "error"}), SYSTEM_AUDIO_UNAVAILABLE),
            (
                json!({"system_audio": "error", "system_audio_error": null}),
                SYSTEM_AUDIO_UNAVAILABLE,
            ),
        ];
        for (session, expected) in cases {
            let message = meeting_capture_status(&session);
            assert_eq!(message, expected, "{session}");
            // Deduplicated: "mic" once, never the raw error text.
            assert_eq!(message.to_lowercase().matches("mic").count(), 1);
            assert!(!message.contains("ScreenCaptureKit") && !message.contains('—'));
            let error = session
                .get("system_audio_error")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !error.to_lowercase().contains("permission") {
                assert!(!message.contains("Allow"), "{session}");
            }
        }
        assert!(system_audio_is_permission_error("TCC denied"));
        assert!(!system_audio_is_permission_error(""));
    }

    #[test]
    fn saved_status_names_the_note_not_its_path() {
        let cases = [
            (
                json!({"notes_path": "/v/notes/meetings/Zoom/2026-11-15 1430 Planning.md"}),
                "Notes saved: Planning",
            ),
            (
                json!({"notes_path": "/v/notes/meetings/Zoom/2026-11-15 1430 Planning.md", "title": "Q4 Planning", "enhanced": true}),
                "Notes saved: Q4 Planning",
            ),
            (
                json!({"notes_path": "/v/notes/Retro.md"}),
                "Notes saved: Retro",
            ),
            (json!({"notes_path": ""}), "Notes saved: Meeting"),
            (json!({}), "Notes saved: Meeting"),
            (
                json!({"title": "   ", "notes_path": "/v/x/Standup.md"}),
                "Notes saved: Standup",
            ),
        ];
        for (saved, expected) in cases {
            let message = saved_notes_message(&saved);
            assert_eq!(message, expected);
            assert!(!message.contains('/') && !message.contains(".md"));
        }
        // A stamp-only name keeps its stem (as Python's Path.stem has it).
        assert_eq!(
            note_title_from_path("/v/2026-11-15 1430 .md"),
            "2026-11-15 1430 "
        );
        assert_eq!(note_title_from_path("/v/.hidden"), ".hidden");
    }

    // ---- recent notes ------------------------------------------------------

    fn note(
        root: &Path,
        category: &str,
        stem: &str,
        heading: Option<&str>,
        body: &str,
        mtime: Option<u64>,
    ) -> PathBuf {
        let folder = root.join(category);
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{stem}.md"));
        let mut text = String::from("---\ndate: x\ntags: [meeting, localflow]\n---\n\n");
        if let Some(heading) = heading {
            text.push_str(&format!("# {heading}\n\n"));
        }
        fs::write(&path, text + body).unwrap();
        if let Some(seconds) = mtime {
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        }
        path
    }

    fn vault_fixture(tmp: &Path) -> PathBuf {
        let vault = tmp.join("vault");
        let notes = vault.join("notes/meetings");
        let titles = [
            "Design review",
            "Hiring sync",
            "Customer call",
            "Standup",
            "Retro",
            "Planning",
        ];
        for (offset, title) in titles.iter().enumerate() {
            let day = 10 + offset;
            note(
                &notes,
                "Zoom",
                &format!("2026-11-{day} 1{offset}00 {title}"),
                Some(title),
                "Body.\n",
                None,
            );
        }
        // Transcripts and files outside notes_folder are never listed.
        note(
            &notes,
            "Zoom",
            "2026-11-20 0900 Planning (transcript)",
            Some("T"),
            "Body.\n",
            None,
        );
        note(
            &vault.join("notes/transcripts"),
            "Zoom",
            "2026-11-21 0900 X (transcript)",
            None,
            "Body.\n",
            None,
        );
        fs::write(vault.join("Inbox.md"), "# Not a meeting\n").unwrap();
        vault
    }

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn recent_notes_lists_the_newest_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = vault_fixture(tmp.path());
        let v = vault.to_str().unwrap();
        let (notes, error) = recent_notes(v, "notes/meetings", 5, Some(at(2026, 11, 15, 18, 0)));
        assert_eq!(error, "");
        let titles: Vec<_> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Planning",
                "Retro",
                "Standup",
                "Customer call",
                "Hiring sync"
            ]
        );
        assert_eq!(notes[0].date_label, "Today, 3:00 PM");
        assert_eq!(notes[1].date_label, "Nov 14, 2:00 PM");
        assert!(notes
            .iter()
            .all(|n| n.path.ends_with(".md") && !n.path.contains("(transcript)")));
        assert_eq!(recent_notes(v, "notes/meetings", 6, None).0.len(), 6);
        assert_eq!(recent_notes(v, "notes/meetings", 0, None).0.len(), 0);
    }

    #[test]
    fn recent_notes_prefers_the_saved_heading_and_strips_the_date() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        let root = vault.join("n");
        note(
            &root,
            "Meet",
            "2026-11-01 0900 Weekly sync",
            Some("Weekly sync: Q4 goals"),
            "Body\n",
            None,
        );
        note(
            &root,
            "Meet",
            "2026-11-02 0900 No heading",
            None,
            "Text\n",
            None,
        );
        note(&root, "Other", "Loose note", None, "", Some(1_000_000));
        let (notes, _) = recent_notes(vault.to_str().unwrap(), "n", 5, None);
        let titles: Vec<_> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(&titles[..2], ["No heading", "Weekly sync: Q4 goals"]);
        assert!(titles.contains(&"Loose note"));
        assert!(!titles.iter().any(|t| t.starts_with("2026-")));
    }

    #[test]
    fn recent_notes_handles_unicode_titles_and_long_files() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        let root = vault.join("n");
        note(
            &root,
            "Café",
            "2026-11-01 0900 Überprüfung 日本語",
            Some("Überprüfung 日本語 🎉"),
            "x\n",
            None,
        );
        // A heading after 4096 characters of filler is not found (Python reads
        // 4096 characters, not bytes): the filename fallback is used.
        let filler = "日".repeat(4100);
        let path = root.join("Café/2026-11-02 0900 Late heading.md");
        fs::write(&path, format!("{filler}\n# Hidden\n")).unwrap();
        let (notes, _) = recent_notes(vault.to_str().unwrap(), "n", 5, None);
        assert_eq!(notes[0].title, "Late heading");
        assert_eq!(notes[1].title, "Überprüfung 日本語 🎉");
        // A heading inside the first 4096 characters is still found.
        fs::write(&path, format!("{}\n# Found\n", " ".repeat(3000))).unwrap();
        let (notes, _) = recent_notes(vault.to_str().unwrap(), "n", 5, None);
        assert_eq!(notes[0].title, "Found");
    }

    #[test]
    fn recent_notes_is_friendly_when_missing_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        assert_eq!(
            recent_notes(t.join("nope").to_str().unwrap(), "notes/meetings", 5, None),
            (vec![], String::new())
        );
        fs::create_dir_all(t.join("v/notes/meetings")).unwrap();
        let v = t.join("v");
        assert_eq!(
            recent_notes(v.to_str().unwrap(), "notes/meetings", 5, None),
            (vec![], String::new())
        );
        assert_eq!(
            recent_notes("", "notes/meetings", 5, None),
            (vec![], String::new())
        );
        assert_eq!(
            recent_notes(v.to_str().unwrap(), "", 5, None),
            (vec![], String::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn recent_notes_reports_an_unreadable_folder_without_panicking() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // permission bits are not enforced for root
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("v/notes/meetings");
        note(&root, "Zoom", "2026-11-01 0900 A", Some("A"), "x\n", None);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
        let result = recent_notes(
            tmp.path().join("v").to_str().unwrap(),
            "notes/meetings",
            5,
            None,
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, (vec![], RECENT_NOTES_UNREADABLE.to_string()));
    }

    #[test]
    fn recent_notes_reports_a_notes_folder_that_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        fs::create_dir_all(&vault).unwrap();
        fs::write(vault.join("notes"), "not a folder").unwrap();
        assert_eq!(
            recent_notes(vault.to_str().unwrap(), "notes", 5, None),
            (vec![], RECENT_NOTES_UNREADABLE.to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn recent_notes_skips_symlinks_pointing_outside() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        let root = vault.join("notes/meetings");
        let good = note(
            &root,
            "Zoom",
            "2026-11-01 0900 Good",
            Some("Good"),
            "x\n",
            None,
        );
        let outside = tmp.path().join("elsewhere");
        let secret = note(
            &outside,
            "Secret",
            "2026-12-01 0900 Secret",
            Some("Secret"),
            "x\n",
            None,
        );
        symlink(outside.join("Secret"), root.join("linked-dir")).unwrap();
        symlink(&secret, root.join("Zoom/2026-12-02 0900 Link.md")).unwrap();
        let (notes, error) = recent_notes(vault.to_str().unwrap(), "notes/meetings", 5, None);
        assert_eq!(error, "");
        let paths: Vec<_> = notes.iter().map(|n| n.path.clone()).collect();
        assert_eq!(
            paths,
            [good.canonicalize().unwrap().to_string_lossy().into_owned()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn recent_notes_never_reads_a_folder_outside_the_vault() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        let outside = tmp.path().join("outside");
        note(
            &outside,
            "Zoom",
            "2026-11-01 0900 Secret",
            Some("Secret"),
            "x\n",
            None,
        );
        let outside_error = (vec![], RECENT_NOTES_OUTSIDE.to_string());
        let v = vault.to_str().unwrap();
        // Absolute notes_folder and a ../ escape.
        assert_eq!(
            recent_notes(v, outside.to_str().unwrap(), 5, None),
            outside_error
        );
        assert_eq!(recent_notes(v, "../outside", 5, None), outside_error);
        // The notes folder itself is a symlink pointing out of the vault.
        symlink(&outside, vault.join("notes")).unwrap();
        assert_eq!(recent_notes(v, "notes", 5, None), outside_error);
    }

    #[cfg(unix)]
    #[test]
    fn recent_notes_turns_a_symlink_loop_into_a_friendly_message() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        fs::create_dir_all(&vault).unwrap();
        symlink(vault.join("a"), vault.join("b")).unwrap();
        symlink(vault.join("b"), vault.join("a")).unwrap();
        assert_eq!(
            recent_notes(vault.to_str().unwrap(), "a", 5, None),
            (vec![], RECENT_NOTES_UNREADABLE.to_string())
        );
    }

    #[test]
    fn recent_notes_depth_hidden_folders_and_dates() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("v");
        let root = vault.join("n");
        note(
            &root,
            "a",
            "2026-11-01 0900 Shallow",
            Some("Shallow"),
            "",
            None,
        );
        note(
            &root,
            ".hidden",
            "2026-11-03 0900 Hidden",
            Some("Hidden"),
            "",
            None,
        );
        // Files at depth 3 are read; directories below that are not entered.
        note(
            &root,
            "a/b/c/d",
            "2026-11-04 0900 Too deep",
            Some("Too deep"),
            "",
            None,
        );
        note(
            &root,
            "a/b",
            "2026-11-05 0900 Deep enough",
            Some("Deep enough"),
            "",
            None,
        );
        // Impossible calendar date -> falls back to the modification time.
        note(
            &root,
            "a",
            "2026-13-45 2500 Bad stamp",
            Some("Bad stamp"),
            "",
            Some(1_000_000),
        );
        let (notes, _) = recent_notes(vault.to_str().unwrap(), "n", 10, Some(at(2027, 1, 1, 0, 0)));
        let mut titles: Vec<_> = notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles[0], "Deep enough");
        titles.sort_unstable();
        assert_eq!(titles, ["Bad stamp", "Deep enough", "Shallow"]);
        let shallow = notes.iter().find(|n| n.title == "Shallow").unwrap();
        assert_eq!(shallow.date_label, "Nov 1, 2026");
    }

    #[test]
    fn note_date_label_formats() {
        let now = at(2026, 11, 15, 18, 0);
        assert_eq!(
            note_date_label(at(2026, 11, 15, 0, 5), Some(now)),
            "Today, 12:05 AM"
        );
        assert_eq!(
            note_date_label(at(2026, 11, 15, 12, 0), Some(now)),
            "Today, 12:00 PM"
        );
        assert_eq!(
            note_date_label(at(2026, 3, 2, 9, 30), Some(now)),
            "Mar 2, 9:30 AM"
        );
        assert_eq!(
            note_date_label(at(2025, 12, 31, 23, 59), Some(now)),
            "Dec 31, 2025"
        );
    }

    #[test]
    fn heading_extraction_edge_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("n.md");
        let cases: [(&str, &str); 8] = [
            ("---\na: b\n---\n\n# Title\n", "Title"),
            ("# Plain\n", "Plain"),
            ("Intro first\n# Late\n", ""),
            ("---\nunterminated\n# Not frontmatter-skipped\n", ""),
            ("\n\n# After blanks\n", "After blanks"),
            ("#NoSpace\n", ""),
            ("# Padded   \r\nbody", "Padded"),
            ("", ""),
        ];
        for (text, expected) in cases {
            fs::write(&path, text).unwrap();
            assert_eq!(note_heading(&path), expected, "{text:?}");
        }
        assert_eq!(note_heading(&tmp.path().join("missing.md")), "");
    }

    // ---- style / hotkeys ----------------------------------------------------

    #[test]
    fn style_rows_never_overlap() {
        for width in WIDTHS {
            let group_width = 560.min(width - 180);
            let mut previous_bottom = 0;
            for index in 0..4 {
                let y = 58 + index * STYLE_ROW_H;
                let frames = style_row_frames(y, group_width);
                assert!(!overlaps(frames.label, frames.example));
                assert!(!overlaps(frames.example, frames.group));
                assert!(frames.example.0 + frames.example.2 < frames.group.0);
                assert!(frames.group.0 + frames.group.2 <= width);
                let bottom = frames.all().iter().map(|f| f.1 + f.3).max().unwrap();
                assert!(bottom <= y + STYLE_ROW_H && y >= previous_bottom);
                previous_bottom = bottom;
                // Two 11pt lines fit the example frame.
                assert!(frames.example.3 >= 2 * 15);
            }
        }
    }

    #[test]
    fn style_update_is_accepted_by_the_shared_config_validation() {
        use lf_core::config::apply_config_update;
        let config = Config::defaults_for(false);
        let update = style_update(
            "medium",
            &[
                ("work", "casual"),
                ("email", "formal"),
                ("personal", "very_casual"),
                ("other", "excited"),
            ],
        );
        assert_eq!(update["cleanup"]["level"], "medium");
        assert_eq!(update["cleanup"]["style_work"], "casual");
        let applied = apply_config_update(&config, &update).unwrap();
        assert_eq!(applied.field_str("cleanup_level"), Some("medium"));
        assert_eq!(applied.field_str("style_work"), Some("casual"));
        assert_eq!(applied.field_str("style_other"), Some("excited"));
        // Validation still runs: nothing is committed for a bad value.
        let bad = style_update("light", &[("work", "shouting")]);
        let error = apply_config_update(&config, &bad).unwrap_err().to_string();
        assert!(
            error.contains("invalid value for config key 'cleanup.style_work'"),
            "{error}"
        );
        assert!(!error.contains("/private"));
        assert!(!STYLE_SAVE_FAILED.contains("shouting"));
        assert_eq!(STYLE_AUTOSAVE_HINT, "Changes save automatically.");
    }

    #[test]
    fn hotkeys_are_formatted_for_humans() {
        assert_eq!(format_hotkey(None), "Off");
        assert_eq!(format_hotkey(Some("  ")), "Off");
        assert_eq!(format_hotkey(Some("alt_r")), "Right Option");
        assert_eq!(
            format_hotkey(Some("<ctrl>+<alt>+2")),
            "Control + Option + 2"
        );
        assert_eq!(
            format_hotkey(Some("<cmd>+<shift>+k")),
            "Command + Shift + K"
        );
        assert_eq!(format_hotkey(Some("<f5>")), "F5");
        assert_eq!(format_hotkey(Some("page_up")), "Page_Up");
        assert_eq!(format_hotkey(Some("ctrl + ÿ")), "Control + Ÿ");
        assert_eq!(py_title("hello wORLD 3rd"), "Hello World 3Rd");
    }

    #[test]
    fn shortcut_rows_follow_the_config_and_invent_no_enable_flag() {
        let mut config = Config::defaults_for(false);
        config.set_field("prompt_hotkey", json!("<ctrl>+<alt>+2"));
        config.set_field("polish_hotkey", json!(""));
        config.set_field("command_hotkey", json!("alt_r"));
        config.set_field("dictation_toggle_hotkey", json!(""));
        let rows = shortcut_rows(&config);
        let names: Vec<_> = rows.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "Prompt Engineer",
                "Polish",
                "Command Mode",
                "Toggle Dictation"
            ]
        );
        let keys: Vec<_> = rows.iter().map(|r| r.hotkey.as_str()).collect();
        assert_eq!(keys, ["Control + Option + 2", "Off", "Right Option", "Off"]);
        assert!(rows[0].detail.contains("Select rough text"));
        assert_eq!(
            rows[3].detail,
            "Turn push-to-talk on or off without quitting LocalFlow."
        );
        config.set_field("dictation_toggle_hotkey", json!("<ctrl>+<alt>+d"));
        assert_eq!(shortcut_rows(&config)[3].hotkey, "Control + Option + D");
        config.fields.remove("dictation_toggle_hotkey");
        assert_eq!(shortcut_rows(&config)[3].hotkey, "Off");
    }

    // ---- dictionary -----------------------------------------------------------

    fn store() -> (tempfile::TempDir, DictionaryStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = DictionaryStore::new(Some(tmp.path().join("dictionary.json")));
        (tmp, store)
    }

    #[test]
    fn dictionary_entries_are_not_capped() {
        let data = DictionaryData {
            words: (0..25).map(|i| format!("word{i}")).collect(),
            replacements: (0..7)
                .map(|i| lf_core::dictionary::Replacement {
                    from: format!("s{i}"),
                    to: format!("t{i}"),
                })
                .collect(),
        };
        let entries = dictionary_entries(&data);
        assert_eq!(entries.len(), 32);
        assert_eq!(
            entries[0],
            DictionaryRow {
                label: "word0".into(),
                key: "word0".into(),
                kind: EntryKind::Word,
                target: String::new()
            }
        );
        assert_eq!(
            entries[25],
            DictionaryRow {
                label: "s0 → t0".into(),
                key: "s0".into(),
                kind: EntryKind::Replacement,
                target: "t0".into()
            }
        );
        assert!(dictionary_entries(&DictionaryData::default()).is_empty());
    }

    #[test]
    fn empty_word_gets_a_friendly_message_and_changes_nothing() {
        let (_tmp, store) = store();
        assert_eq!(
            submit_word(&store, "", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_word(&store, "   ", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_word(&store, "", Some("Old")).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(store.list(), DictionaryData::default());
    }

    #[test]
    fn word_add_edit_and_duplicate() {
        let (_tmp, store) = store();
        assert_eq!(submit_word(&store, "Kubernetes", None), None);
        assert!(submit_word(&store, "kubernetes", None)
            .unwrap()
            .contains("already in your dictionary"));
        assert_eq!(
            submit_word(&store, "  kubernetes ", None).as_deref(),
            Some("“kubernetes” is already in your dictionary.")
        );
        assert_eq!(submit_word(&store, "K8s", Some("Kubernetes")), None);
        assert_eq!(store.words(), ["K8s"]);
        assert!(submit_word(&store, "x", Some("Gone"))
            .unwrap()
            .contains("removed"));
        // A word never ends up as a replacement.
        assert!(store.replacements().is_empty());
        // Unicode words round-trip.
        assert_eq!(submit_word(&store, "Zürich", None), None);
        assert_eq!(store.words(), ["K8s", "Zürich"]);
    }

    #[test]
    fn replacement_validation_names_the_missing_field() {
        let (_tmp, store) = store();
        assert_eq!(
            submit_replacement(&store, "", "the", None).as_deref(),
            Some("Type what you say first.")
        );
        assert_eq!(
            submit_replacement(&store, "teh", "", None).as_deref(),
            Some("Type what it should become.")
        );
        assert_eq!(store.list(), DictionaryData::default());
        assert_eq!(submit_replacement(&store, "teh", "the", None), None);
        assert_eq!(
            store.replacements(),
            [("teh".to_string(), "the".to_string())]
        );
        assert!(store.words().is_empty());
        assert!(submit_replacement(&store, "TEH", "the", None)
            .unwrap()
            .contains("already have a replacement"));
        assert_eq!(submit_replacement(&store, "teh", "then", Some("teh")), None);
        assert_eq!(
            store.replacements(),
            [("teh".to_string(), "then".to_string())]
        );
        assert!(submit_replacement(&store, "a", "b", Some("missing"))
            .unwrap()
            .contains("removed or already exists"));
    }

    struct Broken;

    impl DictionaryEditor for Broken {
        fn add_word(&self, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn update_word(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn add_replacement(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn update_replacement(&self, _: &str, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn remove(&self, _: &str, _: Option<&str>) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
    }

    struct Invalid;

    impl DictionaryEditor for Invalid {
        fn add_word(&self, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("word is empty".into())))
        }
        fn update_word(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("word is empty".into())))
        }
        fn add_replacement(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("empty".into())))
        }
        fn update_replacement(&self, _: &str, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("empty".into())))
        }
        fn remove(&self, _: &str, _: Option<&str>) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn dictionary_errors_are_friendly_not_raw_exceptions() {
        for message in [
            submit_word(&Broken, "x", None),
            submit_word(&Broken, "x", Some("y")),
            submit_replacement(&Broken, "a", "b", None),
            submit_replacement(&Broken, "a", "b", Some("a")),
            remove_entry(&Broken, "x", None),
        ] {
            let message = message.unwrap();
            assert_eq!(message, DICTIONARY_SAVE_FAILED);
            assert!(!message.contains("private") && !message.contains("Something went wrong"));
        }
        // Store-side validation errors map to their own friendly text.
        assert_eq!(
            submit_word(&Invalid, "x", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_replacement(&Invalid, "a", "b", None).as_deref(),
            Some("Fill in both “When I say” and “Type this”.")
        );
        // Removing something that is not there is not an error.
        assert_eq!(remove_entry(&Invalid, "x", None), None);
    }

    #[test]
    fn remove_entry_removes_and_can_target_one_kind() {
        let (_tmp, store) = store();
        store.add_word("Kubernetes").unwrap();
        assert_eq!(remove_entry(&store, "Kubernetes", None), None);
        assert!(store.words().is_empty());
        // Same key as a word and as a replacement: kind decides.
        store.add_word("teh").unwrap();
        store.add_replacement("teh", "the").unwrap();
        assert_eq!(
            remove_entry(&store, "teh", Some(EntryKind::Replacement)),
            None
        );
        assert_eq!(store.words(), ["teh"]);
        assert!(store.replacements().is_empty());
        store.add_replacement("teh", "the").unwrap();
        assert_eq!(remove_entry(&store, "teh", Some(EntryKind::Word)), None);
        assert!(store.words().is_empty());
        assert_eq!(store.replacements().len(), 1);
        assert_eq!(remove_entry(&store, "teh", None), None);
        assert_eq!(store.list(), DictionaryData::default());
    }

    // ---- prompts ----------------------------------------------------------------

    fn two(queue: &FakePromptQueue) -> (PromptEntry, PromptEntry) {
        let made = queue
            .add(
                &[
                    WorkPrompt::new("Migrate sessions", "body"),
                    WorkPrompt::new("Update docs", "body"),
                ],
                &["/tmp/a.md".to_string(), "/tmp/b.md".to_string()],
                "Weekly planning",
            )
            .unwrap();
        (made[0].clone(), made[1].clone())
    }

    fn id(entry: &PromptEntry) -> String {
        entry["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn review_prompt_reports_friendly_messages() {
        let queue = FakePromptQueue::new();
        let (first, other) = two(&queue);
        assert_eq!(
            review_prompt(&queue, &id(&first), ReviewAction::Approve),
            None
        );
        assert_eq!(
            review_prompt(&queue, &id(&other), ReviewAction::Reject),
            None
        );
        assert!(review_prompt(&queue, &id(&first), ReviewAction::Reject)
            .unwrap()
            .contains("changed before it could be rejected"));
        let missing = review_prompt(&queue, "nope", ReviewAction::Approve).unwrap();
        assert!(missing.contains("no longer in the queue"));
        assert!(!missing.contains("unknown prompt"));
    }

    #[test]
    fn stale_click_reports_the_conflict_and_changes_nothing() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue
            .set_status(&id(&entry), "dispatched", None, &Map::new())
            .unwrap();
        let message = review_prompt(&queue, &id(&entry), ReviewAction::Approve).unwrap();
        assert!(message.contains("changed before it could be approved"));
        assert_eq!(
            queue.get(&id(&entry)).unwrap().unwrap()["status"],
            "dispatched"
        );
    }

    #[test]
    fn review_prompt_survives_an_unwritable_queue() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue.set_write_failure(Some("read-only /private/path"));
        let message = review_prompt(&queue, &id(&entry), ReviewAction::Approve).unwrap();
        assert_eq!(
            message,
            "Couldn't update the prompt queue. Check ~/.localflow and try again."
        );
        assert!(!message.contains("private"));
    }

    #[test]
    fn prompt_entries_reports_an_unreadable_queue() {
        let queue = FakePromptQueue::new();
        two(&queue);
        assert_eq!(prompt_entries(&queue).unwrap().len(), 2);
        queue.set_read_failure(Some("corrupt"));
        assert_eq!(prompt_entries(&queue), Err(PROMPTS_UNREADABLE));
    }

    #[test]
    fn prompt_rows_use_repo_then_meeting_and_guard_the_buttons() {
        let entries = vec![
            entry(
                json!({"id": "a", "title": "Pending", "status": "pending", "meeting": "Standup"}),
            ),
            entry(
                json!({"id": "b", "title": "Failed", "status": "failed", "repo": "localflow", "meeting": "Planning", "error": "Orca is not running"}),
            ),
            entry(json!({"id": "c", "title": "Done", "status": "dispatched"})),
            entry(json!({"id": "d", "title": "", "status": "rejected", "meeting": "Retro"})),
        ];
        let rows = prompt_rows(&entries);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["d", "c", "b", "a"]);
        let by = |id: &str| rows.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(by("a").context, "Standup");
        // The stored technical error becomes the shared friendly sentence.
        assert_eq!(
            by("b").context,
            "localflow · Couldn't send — the Orca agent app isn't open"
        );
        assert_eq!(by("b").error, ORCA_NOT_OPEN);
        assert!(!by("b").context.contains("Orca is not running"));
        assert_eq!(by("c").context, "Unknown meeting");
        assert_eq!(by("d").title, "Untitled prompt");
        assert_eq!(by("b").status_label, "Failed");
        let flags: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|k| (by(k).can_approve, by(k).can_reject))
            .collect();
        assert_eq!(
            flags,
            [(true, true), (true, true), (false, false), (false, false)]
        );
        assert!(prompt_rows(&[]).is_empty());
        // A missing status is pending.
        let rows = prompt_rows(&[entry(json!({"id": "z"}))]);
        assert_eq!(
            (rows[0].status.as_str(), rows[0].can_approve),
            ("pending", true)
        );
        assert_eq!(prompt_status_pill("failed"), Some(("red", "red")));
        assert_eq!(prompt_status_pill("pending"), Some(("sun", "ink")));
        assert_eq!(prompt_status_pill("nope"), None);
    }

    #[test]
    fn prompt_errors_map_only_the_known_orca_cause() {
        for stored in [
            "Orca is not running",
            "Orca app is not running — run `orca open`",
            "Orca app is not running — run `orca open` (not possible over SSH: it launches a GUI app on the Mac's own display)",
            "  orca   APP is\nnot running ",
        ] {
            assert_eq!(prompt_error_text(stored), ORCA_NOT_OPEN, "{stored}");
        }
        // Unrelated failures keep their own meaning; nothing is masked.
        for stored in [
            "orca exited 2: repo not found",
            "orca CLI not found on PATH",
            "Orca runtime not reachable (stopped) — run `orca open`",
            "cannot read prompt file /tmp/x.md: [Errno 2] No such file",
            "orca worktree create timed out after 120s",
        ] {
            assert_eq!(prompt_error_text(stored), stored);
        }
        assert_eq!(prompt_error_text(""), "");
        let rows = prompt_rows(&[
            entry(
                json!({"id": "x", "status": "failed", "repo": "r", "error": "orca exited 2: repo not found"}),
            ),
            entry(json!({"id": "y", "status": "pending", "error": "Orca is not running"})),
        ]);
        assert_eq!(rows[1].context, "r · orca exited 2: repo not found");
        assert_eq!(
            (rows[0].context.as_str(), rows[0].error.as_str()),
            ("Unknown meeting", "")
        );
    }

    fn generous(text: &str, size: f64, weight: Weight) -> f64 {
        // A deliberately generous 11pt width estimate (wider than SF Pro).
        assert_eq!((size, weight), (11.0, Weight::Regular));
        text.chars().count() as f64 * 6.8
    }

    #[test]
    fn prompt_rows_wrap_the_failure_instead_of_truncating() {
        for width in [
            HUB_MIN_WIDTH - HUB_CONTENT_X - 32,
            1280 - HUB_CONTENT_X - 32,
        ] {
            let row = prompt_rows(&[entry(json!({
                "id": "f", "status": "failed", "repo": "localflow",
                "error": "Orca is not running", "title": "Migrate"
            }))])
            .remove(0);
            assert!(row.can_approve && row.can_reject);
            let geometry = prompt_row_layout(width, &row, 60, Some(&generous));
            let lines = wrap_lines(&row.context, f64::from(geometry.text_width), &|t| {
                generous(t, 11.0, Weight::Regular)
            });
            // Every wrapped line fits the text column, and the column is tall enough.
            assert!(lines
                .iter()
                .all(|l| generous(l, 11.0, Weight::Regular) <= f64::from(geometry.text_width)));
            assert_eq!(lines.join(" "), row.context);
            assert!(geometry.context_h >= lines.len() as i32 * PROMPT_CONTEXT_LINE);
            assert!(geometry.height >= PROMPT_CONTEXT_Y + geometry.context_h);
            assert_eq!(geometry.lines, lines.len());
            // Approve + Reject + pill + text never overlap and stay inside the row.
            assert!(14 + geometry.text_width <= geometry.pill_x);
            assert!(geometry.pill_x + 60 <= geometry.action_x);
            assert!(geometry.action_x + 88 + 8 + 80 <= width - 12);
            if width < 700 {
                assert!(lines.len() >= 2 && geometry.height > PROMPT_ROW_MIN_H);
            }
        }
    }

    #[test]
    fn prompt_rows_keep_the_compact_height_for_short_context() {
        let row = prompt_rows(&[entry(
            json!({"id": "p", "status": "pending", "meeting": "Standup"}),
        )])
        .remove(0);
        let geometry = prompt_row_layout(600, &row, 60, Some(&generous));
        assert_eq!((geometry.height, geometry.lines), (PROMPT_ROW_MIN_H, 1));
        // Default estimate (no measure) and rows without actions.
        let done = prompt_rows(&[entry(json!({"id": "d", "status": "dispatched"}))]).remove(0);
        let geometry = prompt_row_layout(600, &done, 60, None);
        assert_eq!(geometry.action_x, 600 - PROMPT_EDGE);
        assert_eq!(geometry.height, PROMPT_ROW_MIN_H);
        // A tiny width keeps the 40pt minimum text column.
        assert_eq!(prompt_row_layout(100, &row, 60, None).text_width, 40);
    }

    #[test]
    fn prompt_row_layout_wraps_long_unicode_without_splitting_characters() {
        let long = "日本語のとても長いコンテキスト".repeat(8);
        let row = PromptRow {
            id: "u".into(),
            title: "T".into(),
            context: long.clone(),
            error: String::new(),
            status: "pending".into(),
            status_label: "Pending".into(),
            can_approve: true,
            can_reject: true,
        };
        let measure = |text: &str, _: f64, _: Weight| text.chars().count() as f64 * 11.0;
        let geometry = prompt_row_layout(600, &row, 60, Some(&measure));
        assert!(geometry.lines > 1);
        let lines = wrap_lines(&long, f64::from(geometry.text_width), &|t| {
            measure(t, 11.0, Weight::Regular)
        });
        assert_eq!(lines.concat(), long);
    }

    #[test]
    fn constants_match_the_python_module() {
        assert_eq!(MEETING_FIELD_H, 32);
        assert_eq!(STYLE_ROW_H, 86);
        assert_eq!(PROMPT_ROW_MIN_H, 68);
        assert_eq!(RECENT_ROW_H, 44);
        assert_eq!(
            PROMPTS_EMPTY,
            "Prompts generated from meetings show up here for approval."
        );
        assert!(MEETING_SUBTITLE.starts_with("Transcribe a meeting"));
        assert!(MEETING_NOTES_PLACEHOLDER.contains("LocalFlow fills them in"));
        assert_eq!(MEETING_TITLE_PLACEHOLDER, "Named automatically when saved");
        assert_eq!(
            RECENT_EMPTY,
            "No saved meeting notes yet. Notes you save appear here."
        );
        assert_eq!(py_stem("/a/b/c.tar.gz"), "c.tar");
        assert_eq!(py_stem("/a/b/"), "b");
        assert_eq!(py_stem(""), "");
        assert_eq!(py_splitlines("a\r\nb\rc\u{2028}d\n"), ["a", "b", "c", "d"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
    }
}

// ===== BEGIN lf-ui mac pages: Dictionary/Style page logic (fork P2, append-only) =====

/// Shown when importing a dictionary file fails (`importDictionary_`).
pub const DICTIONARY_IMPORT_FAILED: &str =
    "Couldn't import that file. Choose a CSV or JSON file of words and replacements.";

/// `Imported {added}; duplicates {skipped_duplicate}; invalid {skipped_invalid}.`
pub fn dictionary_import_notice(counts: &lf_core::dictionary::ImportCounts) -> String {
    format!(
        "Imported {}; duplicates {}; invalid {}.",
        counts.added, counts.skipped_duplicate, counts.skipped_invalid
    )
}

/// The dictionary card a submit belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictionaryCard {
    Word,
    Replacement,
}

/// What the Dictionary page keeps across rebuilds: the three typed fields and
/// the entries being edited (`_preserve_page_state` / `_restore_page_state`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictionaryDraft {
    pub word: String,
    pub source: String,
    pub target: String,
    pub editing_word: Option<String>,
    pub editing_source: Option<String>,
}

impl DictionaryDraft {
    /// `_sync_dictionary_buttons`: the add-word button title.
    pub fn word_button_title(&self) -> &'static str {
        if self.editing_word.as_deref().is_some_and(|w| !w.is_empty()) {
            "Save word"
        } else {
            "Add word"
        }
    }

    /// `_sync_dictionary_buttons`: the add-replacement button title.
    pub fn replacement_button_title(&self) -> &'static str {
        if self
            .editing_source
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            "Save replacement"
        } else {
            "Add replacement"
        }
    }

    /// `editDictionary_`: load a row into its card's fields.
    pub fn begin_edit(&mut self, row: &DictionaryRow) {
        match row.kind {
            EntryKind::Word => {
                self.editing_word = Some(row.key.clone());
                self.word = row.key.clone();
            }
            EntryKind::Replacement => {
                self.editing_source = Some(row.key.clone());
                self.source = row.key.clone();
                self.target = row.target.clone();
            }
        }
    }

    /// The success branch of `_reload_dictionary`: clear the submitted card's
    /// fields and drop the editing id of an entry that no longer exists
    /// (compared case-insensitively, restricted to `forget_kind` when given).
    pub fn after_success(
        &mut self,
        clear: Option<DictionaryCard>,
        forget: Option<(&str, Option<EntryKind>)>,
    ) {
        match clear {
            Some(DictionaryCard::Word) => {
                self.word.clear();
                self.editing_word = None;
            }
            Some(DictionaryCard::Replacement) => {
                self.source.clear();
                self.target.clear();
                self.editing_source = None;
            }
            None => {}
        }
        if let Some((key, kind)) = forget.filter(|(key, _)| !key.is_empty()) {
            let same = |editing: &Option<String>| {
                editing.as_deref().is_some_and(|e| {
                    !e.is_empty()
                        && caseless::default_case_fold_str(e)
                            == caseless::default_case_fold_str(key)
                })
            };
            if matches!(kind, None | Some(EntryKind::Word)) && same(&self.editing_word) {
                self.editing_word = None;
            }
            if matches!(kind, None | Some(EntryKind::Replacement)) && same(&self.editing_source) {
                self.editing_source = None;
            }
        }
    }
}

/// Cleanup levels in segment order (`cleanup_options`).
pub const STYLE_CLEANUP_OPTIONS: [&str; 3] = ["none", "light", "medium"];
/// Segment titles of [`STYLE_CLEANUP_OPTIONS`].
pub const STYLE_CLEANUP_TITLES: [&str; 3] = ["None", "Light", "Medium"];
/// Style options in segment order (`style_options`).
pub const STYLE_OPTIONS: [&str; 4] = ["formal", "casual", "very_casual", "excited"];
/// `(category, example)` rows of the Style page, in display order.
pub const STYLE_CATEGORIES: [(&str, &str); 4] = [
    ("work", "Please share the updated timeline."),
    ("email", "Thanks for following up."),
    ("personal", "Let's catch up soon!"),
    ("other", "The main point is clear."),
];

/// `value.replace("_", " ").title()`: `very_casual` -> `Very Casual`.
pub fn style_option_title(value: &str) -> String {
    py_title(&value.replace('_', " "))
}

/// The row label, `f"{category.title()} style"`.
pub fn style_category_label(category: &str) -> String {
    format!("{} style", py_title(category))
}

/// `_style_autosave`'s failure text: a rejected config shows its own message,
/// anything else (an unwritable file) the generic one.
pub fn style_save_error_text(error: &anyhow::Error) -> String {
    match error.downcast_ref::<lf_core::config::ConfigError>() {
        Some(config_error) => config_error.to_string(),
        None => STYLE_SAVE_FAILED.to_string(),
    }
}

#[cfg(test)]
mod p2_page_logic_tests {
    use super::*;

    fn row(kind: EntryKind, key: &str, target: &str) -> DictionaryRow {
        DictionaryRow {
            label: key.into(),
            key: key.into(),
            kind,
            target: target.into(),
        }
    }

    #[test]
    fn edit_loads_the_row_and_switches_button_titles() {
        let mut draft = DictionaryDraft::default();
        assert_eq!(draft.word_button_title(), "Add word");
        assert_eq!(draft.replacement_button_title(), "Add replacement");
        draft.begin_edit(&row(EntryKind::Word, "Kubernetes", ""));
        assert_eq!(
            (draft.word.as_str(), draft.word_button_title()),
            ("Kubernetes", "Save word")
        );
        assert_eq!(draft.replacement_button_title(), "Add replacement");
        draft.begin_edit(&row(EntryKind::Replacement, "my email", "me@example.com"));
        assert_eq!(
            (
                draft.source.as_str(),
                draft.target.as_str(),
                draft.replacement_button_title()
            ),
            ("my email", "me@example.com", "Save replacement")
        );
        // An empty editing id is "not editing", like Python's truthiness.
        draft.editing_word = Some(String::new());
        assert_eq!(draft.word_button_title(), "Add word");
    }

    #[test]
    fn success_clears_only_the_submitted_card() {
        let mut draft = DictionaryDraft {
            word: "w".into(),
            source: "s".into(),
            target: "t".into(),
            editing_word: Some("w".into()),
            editing_source: Some("s".into()),
        };
        draft.after_success(Some(DictionaryCard::Word), None);
        assert_eq!(
            (draft.word.as_str(), draft.editing_word.clone()),
            ("", None)
        );
        assert_eq!((draft.source.as_str(), draft.target.as_str()), ("s", "t"));
        assert_eq!(draft.editing_source.as_deref(), Some("s"));
        draft.after_success(Some(DictionaryCard::Replacement), None);
        assert_eq!(draft, DictionaryDraft::default());
    }

    #[test]
    fn removing_an_edited_entry_forgets_it_case_insensitively_by_kind() {
        let mut draft = DictionaryDraft {
            editing_word: Some("Kubernetes".into()),
            editing_source: Some("kubernetes".into()),
            ..DictionaryDraft::default()
        };
        // A replacement removal leaves the word being edited alone.
        draft.after_success(None, Some(("KUBERNETES", Some(EntryKind::Replacement))));
        assert_eq!(draft.editing_word.as_deref(), Some("Kubernetes"));
        assert_eq!(draft.editing_source, None);
        draft.after_success(None, Some(("kubernetes", Some(EntryKind::Word))));
        assert_eq!(draft.editing_word, None);
        // Unrestricted kind drops both; an empty key forgets nothing; `ß` folds to `ss`.
        let mut both = DictionaryDraft {
            editing_word: Some("Straße".into()),
            editing_source: Some("STRASSE".into()),
            ..DictionaryDraft::default()
        };
        both.after_success(None, Some(("", None)));
        assert!(both.editing_word.is_some() && both.editing_source.is_some());
        both.after_success(None, Some(("strasse", None)));
        assert_eq!((both.editing_word, both.editing_source), (None, None));
    }

    #[test]
    fn import_notice_and_failure_texts_match_python() {
        let counts = lf_core::dictionary::ImportCounts {
            added: 3,
            skipped_duplicate: 1,
            skipped_invalid: 2,
        };
        assert_eq!(
            dictionary_import_notice(&counts),
            "Imported 3; duplicates 1; invalid 2."
        );
        assert_eq!(
            DICTIONARY_IMPORT_FAILED,
            "Couldn't import that file. Choose a CSV or JSON file of words and replacements."
        );
    }

    #[test]
    fn style_titles_labels_and_error_text() {
        assert_eq!(style_option_title("very_casual"), "Very Casual");
        assert_eq!(style_option_title("formal"), "Formal");
        assert_eq!(style_category_label("work"), "Work style");
        assert_eq!(STYLE_CLEANUP_OPTIONS.len(), STYLE_CLEANUP_TITLES.len());
        assert_eq!(STYLE_CATEGORIES[3].0, "other");
        let rejected = anyhow::Error::new(lf_core::config::ConfigError("bad level".into()));
        assert_eq!(style_save_error_text(&rejected), "bad level");
        assert_eq!(
            style_save_error_text(&anyhow::anyhow!("disk full")),
            STYLE_SAVE_FAILED
        );
    }
}
// ===== END lf-ui mac pages: Dictionary/Style page logic (fork P2) =====
