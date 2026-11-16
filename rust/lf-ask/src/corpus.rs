use crate::text::{ascii_digits, lines, python_whitespace};
use crate::{
    chunk_lines, ranking::timestamp, Passage, HISTORY_SCAN_LIMIT, SOURCE_DICTATION, SOURCE_MEETING,
    SOURCE_NOTES,
};
use anyhow::Result;
use chrono::{Local, NaiveDate, NaiveDateTime};
use lf_core::{config::Config, history::HistoryStore, home::expand_user};
use regex::Regex;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

static FILE_STAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4})-(\d{2})-(\d{2}) (\d{2})(\d{2})(?:[\s\x1c-\x1f]+(.*))?\n?\z").unwrap()
});
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*\*\[?(\d+:\d{2}(?::\d{2})?)\]?\*\*").unwrap());

#[derive(Default)]
pub struct Corpus<'a> {
    pub store: Option<&'a HistoryStore>,
    pub vault_path: PathBuf,
    pub notes_folder: String,
    pub logs_folder: String,
}
impl<'a> Corpus<'a> {
    pub fn from_config(config: &Config, store: Option<&'a HistoryStore>) -> Self {
        Self {
            store: if config.field_bool("history_enabled") == Some(false) {
                None
            } else {
                store
            },
            vault_path: config.field_str("vault_path").unwrap_or("").into(),
            notes_folder: config.field_str("notes_folder").unwrap_or("").into(),
            logs_folder: config.field_str("logs_folder").unwrap_or("").into(),
        }
    }
}
pub fn parse_file_stamp(path: &Path) -> (NaiveDateTime, String) {
    let file_stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let stem = file_stem
        .strip_suffix(" (transcript)")
        .unwrap_or(&file_stem);
    if let Some(c) = FILE_STAMP.captures(stem) {
        let parse = || -> Option<NaiveDateTime> {
            let year = ascii_digits(&c[1]).parse().ok()?;
            if year == 0 {
                return None;
            }
            NaiveDate::from_ymd_opt(
                year,
                ascii_digits(&c[2]).parse().ok()?,
                ascii_digits(&c[3]).parse().ok()?,
            )?
            .and_hms_opt(
                ascii_digits(&c[4]).parse().ok()?,
                ascii_digits(&c[5]).parse().ok()?,
                0,
            )
        };
        if let Some(when) = parse() {
            let title = c
                .get(6)
                .map(|m| m.as_str().trim_matches(python_whitespace))
                .filter(|s| !s.is_empty())
                .unwrap_or(stem);
            return (when, title.into());
        }
    }
    let when = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(chrono::DateTime::<Local>::from)
        .and_then(|d| {
            crate::local_time(d.timestamp() as f64 + d.timestamp_subsec_nanos() as f64 / 1e9)
        })
        .map(|d| d.naive_local())
        .unwrap_or_else(|| Local::now().naive_local());
    (when, stem.into())
}
fn strip_frontmatter(text: &str) -> &str {
    if let Some(body) = text.strip_prefix("---\n") {
        if let Some(end) = body.find("\n---\n") {
            return &body[end + 5..];
        }
    }
    text
}
fn read_text(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path).map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
}
pub fn read_transcript_log(path: &Path) -> std::io::Result<String> {
    let text = read_text(path)?;
    let text = strip_frontmatter(&text)
        .split("\n## My raw notes\n")
        .next()
        .unwrap_or("");
    Ok(lines(text.trim_matches(python_whitespace))
        .filter(|line| !line.starts_with("Notes: [["))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches(python_whitespace)
        .into())
}
pub fn transcript_passages(path: &Path) -> Vec<Passage> {
    let Ok(text) = read_transcript_log(path) else {
        return vec![];
    };
    let (started, title) = parse_file_stamp(path);
    let text = lines(&text)
        .filter(|line| !line.starts_with("# "))
        .collect::<Vec<_>>()
        .join("\n");
    chunk_lines(&text)
        .into_iter()
        .map(|text| {
            let stamp = STAMP
                .captures(&text)
                .map(|c| c[1].to_owned())
                .unwrap_or_default();
            let seconds = ascii_digits(&stamp)
                .split(':')
                .try_fold(0_u64, |total, p| {
                    total.checked_mul(60)?.checked_add(p.parse().ok()?)
                })
                .unwrap_or(0);
            let when = i64::try_from(seconds)
                .ok()
                .and_then(chrono::Duration::try_seconds)
                .and_then(|s| started.checked_add_signed(s))
                .unwrap_or(started);
            Passage {
                text,
                source: SOURCE_MEETING.into(),
                title: title.clone(),
                ts: Some(timestamp(when)),
                reference: if stamp.is_empty() {
                    path.to_string_lossy().into()
                } else {
                    format!("{}#{stamp}", path.display())
                },
                app: String::new(),
            }
        })
        .collect()
}
pub fn notes_passages(path: &Path) -> Vec<Passage> {
    let Ok(text) = read_text(path) else {
        return vec![];
    };
    let (started, title) = parse_file_stamp(path);
    chunk_lines(strip_frontmatter(&text))
        .into_iter()
        .map(|text| Passage {
            text,
            source: SOURCE_NOTES.into(),
            title: title.clone(),
            ts: Some(timestamp(started)),
            reference: path.to_string_lossy().into(),
            app: String::new(),
        })
        .collect()
}
fn files(root: &Path, transcripts: bool) -> Vec<PathBuf> {
    fn visit(root: &Path, transcripts: bool, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                visit(&path, transcripts, out);
            } else if path.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let pattern_name = if cfg!(windows) {
                    name.to_lowercase()
                } else {
                    name.to_string()
                };
                if if transcripts {
                    pattern_name.ends_with(" (transcript).md")
                } else {
                    pattern_name.ends_with(".md") && !name.ends_with(" (transcript).md")
                } {
                    out.push(path);
                }
            }
        }
    }
    let mut out = vec![];
    visit(root, transcripts, &mut out);
    if cfg!(windows) {
        out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    } else {
        out.sort();
    }
    out
}
pub fn build_corpus(corpus: &Corpus<'_>) -> Result<Vec<Passage>> {
    let mut passages = Vec::new();
    if let Some(store) = corpus.store.filter(|s| s.enabled) {
        for row in store.recent(HISTORY_SCAN_LIMIT, None, 0)? {
            let text = row.final_text.trim_matches(python_whitespace);
            if row.status != "ok" || text.is_empty() {
                continue;
            }
            let app = row.app_name.unwrap_or_default();
            passages.extend(chunk_lines(text).into_iter().map(|text| Passage {
                text,
                source: SOURCE_DICTATION.into(),
                title: app.clone(),
                ts: Some(row.ts),
                reference: row.id.to_string(),
                app: app.clone(),
            }));
        }
    }
    if !corpus.vault_path.as_os_str().is_empty() {
        let vault = expand_user(&corpus.vault_path)?;
        if !corpus.logs_folder.is_empty() {
            for path in files(&vault.join(&corpus.logs_folder), true) {
                passages.extend(transcript_passages(&path));
            }
        }
        if !corpus.notes_folder.is_empty() {
            for path in files(&vault.join(&corpus.notes_folder), false) {
                passages.extend(notes_passages(&path));
            }
        }
    }
    Ok(passages)
}
