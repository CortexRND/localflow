use std::{collections::HashMap, path::Path};

use anyhow::{anyhow, bail, Result};
use chrono::{Days, Local};
use lf_ask::{
    chunk_lines, is_relative_since, local_time, parse_since, snippet, timestamp, Passage, Since,
    HISTORY_SCAN_LIMIT, SOURCE_DICTATION,
};
use lf_core::{
    history::{HistoryEntry, HistoryReader},
    symbols::pycompat::strip,
};
use serde_json::{json, Number, Value};

use crate::sources::{cut, history_enabled, query_terms, rank_refs, results, substring_snippet};

fn open(path: &Path) -> Result<Option<HistoryReader>> {
    if !history_enabled()? {
        bail!("Dictation history is disabled (`[history] enabled = false`)");
    }
    HistoryReader::open(path)
}

fn bound(value: Option<&str>, name: &str, end: bool) -> Result<Option<f64>> {
    let Some(value) = value.filter(|s| !strip(s).is_empty()) else {
        return Ok(None);
    };
    let parsed = parse_since(Some(&Since::from(value)), Local::now().naive_local())
        .map_err(|e| anyhow!(e.to_string().replacen("since", name, 1)))?;
    let Some(mut local) = parsed else {
        return Ok(None);
    };
    if end && !is_relative_since(value) {
        local = local
            .checked_add_days(Days::new(1))
            .ok_or_else(|| anyhow!("date value out of range"))?;
    }
    Ok(Some(timestamp(local)))
}

fn when(row: &HistoryEntry) -> Result<String> {
    let date = local_time(row.ts).ok_or_else(|| anyhow!("timestamp out of range: {}", row.ts))?;
    Ok(date.format("%Y-%m-%dT%H:%M:%S").to_string())
}

fn duration(row: &HistoryEntry) -> Value {
    if row.duration_s == 0.0 {
        json!(0)
    } else {
        json!(row.duration_s)
    }
}

fn hit(row: &HistoryEntry, snippet: String) -> Result<Value> {
    Ok(json!({
        "id": row.id,
        "when": when(row)?,
        "kind": row.kind,
        "app": row.app_name.as_deref().unwrap_or(""),
        "category": row.category.as_deref().unwrap_or(""),
        "words": row.words,
        "duration_s": duration(row),
        "snippet": snippet,
    }))
}

pub fn search(
    path: &Path,
    query: &str,
    since: Option<&str>,
    until: Option<&str>,
    app: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let start = bound(since, "since", false)?;
    let stop = bound(until, "until", true)?;
    let query = strip(query);
    let Some(reader) = open(path)? else {
        return Ok(results(
            if query.is_empty() { "recent" } else { "ranked" },
            vec![],
        ));
    };
    let app = app.map(strip).filter(|s| !s.is_empty());
    let rows: Vec<_> = reader
        .recent_ok(start, stop, app, kind, HISTORY_SCAN_LIMIT)?
        .into_iter()
        .filter(|r| !strip(&r.final_text).is_empty() || !strip(&r.raw_text).is_empty())
        .collect();
    if query.is_empty() {
        let hits = rows.iter().take(limit).map(|r| {
            let final_text = strip(&r.final_text);
            let text = if final_text.is_empty() {
                strip(&r.raw_text)
            } else {
                final_text
            };
            hit(r, snippet(text, &Default::default()))
        });
        return Ok(results("recent", hits.collect::<Result<_>>()?));
    }

    let by_id: HashMap<_, _> = rows.iter().map(|r| (r.id.to_string(), r)).collect();
    let mut passages = Vec::new();
    for row in &rows {
        let final_text = strip(&row.final_text);
        let raw_text = strip(&row.raw_text);
        let app = row.app_name.as_deref().unwrap_or("");
        let texts = std::iter::once(final_text).chain((raw_text != final_text).then_some(raw_text));
        for text in texts.flat_map(chunk_lines) {
            passages.push(Passage {
                text,
                source: SOURCE_DICTATION.into(),
                title: app.into(),
                app: app.into(),
                ts: Some(row.ts),
                reference: row.id.to_string(),
            });
        }
    }
    let top = rank_refs(query, &passages, limit)?;
    if !top.is_empty() {
        let terms = query_terms(query);
        let hits = top
            .iter()
            .map(|p| hit(by_id[&p.reference], snippet(&p.text, &terms)));
        return Ok(results("ranked", hits.collect::<Result<_>>()?));
    }

    let needle = strip(&query.replace('"', "")).to_lowercase();
    let mut hits = Vec::new();
    if !needle.is_empty() {
        for row in &rows {
            if let Some(text) = [&row.final_text, &row.raw_text]
                .into_iter()
                .find(|s| s.to_lowercase().contains(&needle))
            {
                hits.push(hit(row, substring_snippet(text, &needle))?);
                if hits.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(results("substring", hits))
}

pub fn get(path: &Path, id: &Number) -> Result<Value> {
    let missing = || anyhow!("no dictation with id {id}");
    let reader = open(path)?.ok_or_else(missing)?;
    let id = id.as_i64().ok_or_else(|| {
        anyhow!("get_dictation failed: Python int too large to convert to SQLite INTEGER")
    })?;
    let row = reader.get(id)?.ok_or_else(missing)?;
    let (final_text, cut_final) = cut(&row.final_text);
    let (raw_text, cut_raw) = cut(&row.raw_text);
    Ok(json!({
        "id": row.id,
        "when": when(&row)?,
        "kind": row.kind,
        "status": row.status,
        "app": row.app_name.as_deref().unwrap_or(""),
        "bundle_id": row.bundle_id.as_deref().unwrap_or(""),
        "category": row.category.as_deref().unwrap_or(""),
        "words": row.words,
        "duration_s": duration(&row),
        "final_text": final_text,
        "raw_text": raw_text,
        "truncated": cut_final || cut_raw,
    }))
}
