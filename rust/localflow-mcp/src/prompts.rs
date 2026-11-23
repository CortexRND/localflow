use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::Path,
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use lf_ask::{chunk_lines, snippet, timestamp, Passage};
use lf_core::{
    home::expand_user,
    symbols::pycompat::{repr_str, strip, truthy},
};
use serde_json::{json, Map, Value};

use crate::sources::{cut, query_terms, rank_refs, results, substring_snippet};

pub const STATUSES: [&str; 5] = ["pending", "approved", "rejected", "dispatched", "failed"];
const MAX_PROMPT_FILE_BYTES: u64 = 256 * 1024;
type Entry = Map<String, Value>;

fn read_queue(path: &Path) -> Vec<Entry> {
    let Ok(data) = fs::read(path) else {
        return vec![];
    };
    let Ok(Value::Array(entries)) = serde_json::from_slice(&data) else {
        return vec![];
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            Value::Object(entry) if entry.get("id").is_some_and(Value::is_string) => Some(entry),
            _ => None,
        })
        .collect()
}

fn strip_frontmatter(text: &str) -> &str {
    if let Some(header) = text.strip_prefix("---\n") {
        let mut rest = header;
        for line in header.split_inclusive('\n') {
            rest = &rest[line.len()..];
            if line.trim_end_matches('\n') == "---" {
                return rest.trim_start_matches('\n');
            }
        }
    }
    text
}

fn read_body(entry: &Entry) -> Option<String> {
    let path = expand_user(Path::new(text(entry, "path"))).ok()?;
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        || !path.is_file()
    {
        return None;
    }
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_PROMPT_FILE_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_PROMPT_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_PROMPT_FILE_BYTES {
        return None;
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Some(strip_frontmatter(&text).to_string())
}

fn text<'a>(entry: &'a Entry, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

fn field(entry: &Entry, key: &str) -> Value {
    entry
        .get(key)
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!(""))
}

fn created_ts(text: &str) -> Option<f64> {
    let normalized = text.replacen(' ', "T", 1);
    if let Ok(time) = DateTime::parse_from_rfc3339(&normalized) {
        return Some(time.timestamp() as f64 + f64::from(time.timestamp_subsec_micros()) / 1e6);
    }
    NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M"))
        .or_else(|_| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        })
        .ok()
        .map(timestamp)
}

fn hit(entry: &Entry, snippet: String) -> Value {
    json!({
        "id": entry["id"],
        "title": field(entry, "title"),
        "meeting": field(entry, "meeting"),
        "status": field(entry, "status"),
        "created": field(entry, "created"),
        "snippet": snippet,
    })
}

pub fn search(path: &Path, query: &str, status: Option<&str>, limit: usize) -> Result<Value> {
    let mut entries = read_queue(path);
    if let Some(status) = status {
        entries.retain(|e| text(e, "status") == status);
    }
    entries.sort_by(|a, b| text(b, "created").cmp(text(a, "created")));
    let query = strip(query);
    if query.is_empty() {
        let hits = entries
            .iter()
            .take(limit)
            .map(|e| {
                hit(
                    e,
                    snippet(&read_body(e).unwrap_or_default(), &Default::default()),
                )
            })
            .collect();
        return Ok(results("recent", hits));
    }

    let by_id: HashMap<_, _> = entries.iter().map(|e| (text(e, "id"), e)).collect();
    let texts: HashMap<_, _> = entries
        .iter()
        .map(|e| {
            let body = read_body(e).unwrap_or_default();
            let full = [text(e, "title"), text(e, "meeting"), &body]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (text(e, "id"), full)
        })
        .collect();
    let mut passages = Vec::new();
    for e in &entries {
        let id = text(e, "id");
        let ts = created_ts(text(e, "created"));
        for body in chunk_lines(&texts[id]) {
            passages.push(Passage {
                text: body,
                source: "prompt".into(),
                title: text(e, "title").into(),
                app: String::new(),
                ts,
                reference: id.into(),
            });
        }
    }
    let top = rank_refs(query, &passages, limit)?;
    if !top.is_empty() {
        let terms = query_terms(query);
        let hits = top
            .iter()
            .map(|p| hit(by_id[p.reference.as_str()], snippet(&p.text, &terms)))
            .collect();
        return Ok(results("ranked", hits));
    }

    let needle = strip(&query.replace('"', "")).to_lowercase();
    let hits = entries
        .iter()
        .filter_map(|e| {
            let full = &texts[text(e, "id")];
            (!needle.is_empty() && full.to_lowercase().contains(&needle))
                .then(|| hit(e, substring_snippet(full, &needle)))
        })
        .take(limit)
        .collect();
    Ok(results("substring", hits))
}

pub fn get(path: &Path, id: &str) -> Result<Value> {
    let entry = read_queue(path)
        .into_iter()
        .find(|e| text(e, "id") == id)
        .ok_or_else(|| anyhow!("no work prompt with id {}", repr_str(id)))?;
    let body = read_body(&entry);
    let (body_text, truncated) = cut(body.as_deref().unwrap_or(""));
    let mut out = json!({
        "id": id,
        "title": field(&entry, "title"),
        "meeting": field(&entry, "meeting"),
        "status": field(&entry, "status"),
        "created": field(&entry, "created"),
        "path": field(&entry, "path"),
        "worktree": field(&entry, "worktree"),
        "repo": field(&entry, "repo"),
        "error": field(&entry, "error"),
        "body": body_text,
        "truncated": truncated,
    });
    if body.is_none() {
        out["note"] = json!("prompt file missing");
    }
    Ok(out)
}
