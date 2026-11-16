//! Run against a prepared history.sqlite and vault; use /usr/bin/time for process RSS.
//! cargo run --release -p lf-ask --example bench_search -- DB VAULT [ITERATIONS]
use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use lf_ask::{search, Corpus, SearchOptions};
use lf_core::history::HistoryStore;
use serde_json::json;
use std::{env, time::Instant};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let db = args.next().context("DB required")?;
    let vault = args.next().context("VAULT required")?;
    let iterations: usize = args.next().unwrap_or_else(|| "1".into()).parse()?;
    anyhow::ensure!(iterations > 0, "ITERATIONS must be positive");
    let store = HistoryStore::open(Some(db.into()), None, true)?;
    let corpus = Corpus {
        store: Some(&store),
        vault_path: vault.into(),
        notes_folder: "Notes".into(),
        logs_folder: "Transcripts".into(),
    };
    let options = SearchOptions {
        now: NaiveDateTime::parse_from_str("2026-11-14T10:00:00", "%Y-%m-%dT%H:%M:%S")?,
        ..Default::default()
    };
    let mut milliseconds = Vec::new();
    let mut citations = Vec::new();
    for _ in 0..iterations {
        let start = Instant::now();
        let result = search("launch pricing budget", &corpus, &options)?;
        milliseconds.push(start.elapsed().as_secs_f64() * 1000.0);
        citations = result.citations;
    }
    println!(
        "{}",
        json!({"milliseconds": milliseconds, "citations": citations})
    );
    Ok(())
}
