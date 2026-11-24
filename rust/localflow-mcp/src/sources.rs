use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use lf_ask::{extract_phrases, rank, tokenize, Passage, SearchOptions, SNIPPET_CHARS};
use lf_core::{
    config::{self, Config},
    history,
    home::localflow_dir,
    symbols::pycompat::split_ws,
};
use serde_json::{json, Value};

/// The launching app's cwd, the base for relative queue prompt paths.
pub const PROMPTS_CWD_ENV: &str = "LOCALFLOW_MCP_PROMPTS_CWD";

pub struct Sources {
    pub history: PathBuf,
    pub queue: PathBuf,
    /// [`PROMPTS_CWD_ENV`]; `None` (unset) keeps this process's cwd.
    pub prompts_cwd: Option<OsString>,
}

impl Sources {
    pub fn local() -> Self {
        Self {
            history: history::history_path(),
            queue: localflow_dir().join("queue.json"),
            prompts_cwd: std::env::var_os(PROMPTS_CWD_ENV),
        }
    }
}

pub fn history_enabled() -> Result<bool> {
    let target = config::try_config_path()?;
    let darwin = cfg!(target_os = "macos");
    let config = if target.try_exists()? {
        config::load_from(&target)?
    } else {
        let legacy = config::try_legacy_config_path()?;
        if legacy.try_exists()? {
            let (migrated, _) = config::migrate_v1(&config::parse_toml_file(&legacy)?);
            config::config_from_v2(&migrated, darwin)?.0
        } else {
            Config::defaults_for(darwin)
        }
    };
    Ok(config.field_bool("history_enabled").unwrap_or(true))
}

pub fn cut(text: &str) -> (&str, bool) {
    const MAX_TEXT_CHARS: usize = 20_000;
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((at, _)) => (&text[..at], true),
        None => (text, false),
    }
}

pub fn results(kind: &str, results: Vec<Value>) -> Value {
    json!({"count": results.len(), "match": kind, "results": results})
}

pub fn query_terms(query: &str) -> HashSet<String> {
    let (plain, _) = extract_phrases(query);
    tokenize(&plain, false).into_iter().collect()
}

pub fn rank_refs(query: &str, passages: &[Passage], limit: usize) -> Result<Vec<Passage>> {
    let ranked = rank(
        query,
        passages,
        &SearchOptions {
            k: passages.len(),
            ..Default::default()
        },
    )?;
    let mut seen = HashSet::new();
    Ok(ranked
        .into_iter()
        .filter(|p| seen.insert(p.reference.clone()))
        .take(limit)
        .collect())
}

pub fn substring_snippet(text: &str, needle: &str) -> String {
    let flat = split_ws(text).join(" ");
    let len = flat.chars().count();
    if len <= SNIPPET_CHARS {
        return flat;
    }
    let lower = flat.to_lowercase();
    let start = lower
        .find(needle)
        .map_or(0, |at| lower[..at].chars().count().saturating_sub(60));
    let piece: String = flat.chars().skip(start).take(SNIPPET_CHARS).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        piece.trim(),
        if start + SNIPPET_CHARS < len {
            "…"
        } else {
            ""
        }
    )
}
