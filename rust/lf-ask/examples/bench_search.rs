//! Recall search timing against a prepared history.sqlite and vault.
//!
//! Legacy (one fixed query, prints `{"citations": [...], "milliseconds": [...]}`):
//!   cargo run --release -p lf-ask --example bench_search -- DB VAULT [ITERATIONS]
//!
//! Query set (used by `evals/perf/suites/recall.py`; same JSON as
//! `evals/perf/benches/py_recall.py`):
//!   bench_search DB VAULT --queries FILE [--iterations N] [--warmup N] [--now ISO]
//!
//! `FILE` is a JSON list of query strings or `{"query": str, "since": str}`
//! objects, or newline-delimited plain queries. Each query runs `--warmup`
//! discarded (default 1) and `--iterations` timed (default 5) searches; the
//! output is one JSON object with `queries` (keys in file order),
//! `milliseconds` (key -> timed ms), `all_milliseconds`, `citations_by_query`
//! (key -> citations, vault paths in `ref` made vault-relative with `/`) and
//! `peak_rss_bytes` (process peak RSS, or null when unavailable).
use anyhow::{bail, ensure, Context, Result};
use chrono::NaiveDateTime;
use lf_ask::{search, Corpus, SearchOptions, Since};
use lf_core::history::HistoryStore;
use serde_json::{json, Map, Value};
use std::{env, fs, path::PathBuf, time::Instant};

const LEGACY_QUERY: &str = "launch pricing budget";
const DEFAULT_NOW: &str = "2026-11-14T10:00:00";
const DEFAULT_QUERY_ITERATIONS: usize = 5;
const DEFAULT_QUERY_WARMUP: usize = 1;

#[derive(Debug, PartialEq)]
struct Args {
    db: PathBuf,
    vault: PathBuf,
    queries: Option<PathBuf>,
    iterations: usize,
    warmup: usize,
    now: String,
}

#[derive(Debug, PartialEq)]
struct Query {
    query: String,
    since: Option<String>,
}

impl Query {
    fn key(&self) -> String {
        match &self.since {
            Some(since) => format!("{} (since {since})", self.query),
            None => self.query.clone(),
        }
    }
}

fn parse_count(name: &str, value: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("{name} must be a non-negative integer, got {value:?}"))
}

fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut positional = Vec::new();
    let (mut queries, mut iterations, mut warmup, mut now) = (None, None, None, None);
    let mut raw = raw.into_iter();
    while let Some(arg) = raw.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        if !flag.starts_with("--") || flag == "--" {
            positional.push(arg);
            continue;
        }
        let mut value = || -> Result<String> {
            inline
                .clone()
                .or_else(|| raw.next())
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--queries" => queries = Some(PathBuf::from(value()?)),
            "--iterations" => iterations = Some(parse_count("--iterations", &value()?)?),
            "--warmup" => warmup = Some(parse_count("--warmup", &value()?)?),
            "--now" => now = Some(value()?),
            other => bail!("unknown flag {other}"),
        }
    }
    let mut positional = positional.into_iter();
    let db = positional.next().context("DB required")?;
    let vault = positional.next().context("VAULT required")?;
    if let Some(n) = positional.next() {
        ensure!(
            iterations.is_none(),
            "give ITERATIONS or --iterations, not both"
        );
        iterations = Some(parse_count("ITERATIONS", &n)?);
    }
    if let Some(extra) = positional.next() {
        bail!("unexpected argument {extra:?}");
    }
    let query_mode = queries.is_some();
    let iterations = iterations.unwrap_or(if query_mode {
        DEFAULT_QUERY_ITERATIONS
    } else {
        1
    });
    ensure!(iterations > 0, "ITERATIONS must be positive");
    let now = now.unwrap_or_else(|| DEFAULT_NOW.into());
    parse_now(&now)?;
    Ok(Args {
        db: db.into(),
        vault: vault.into(),
        queries,
        iterations,
        warmup: warmup.unwrap_or(if query_mode { DEFAULT_QUERY_WARMUP } else { 0 }),
        now,
    })
}

fn parse_now(text: &str) -> Result<NaiveDateTime> {
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(text, f).ok())
    .with_context(|| format!("--now must be YYYY-MM-DDTHH:MM[:SS], got {text:?}"))
}

/// Parse a query file: a JSON list (strings or `{"query", "since"}` objects)
/// or newline-delimited plain queries (blank lines skipped).
fn parse_queries(text: &str) -> Result<Vec<Query>> {
    let text = text.trim_start_matches('\u{feff}').trim();
    let queries: Vec<Query> = if text.starts_with('[') {
        let items: Vec<Value> = serde_json::from_str(text).context("invalid queries JSON")?;
        items
            .into_iter()
            .map(|item| match item {
                Value::String(query) => Ok(Query { query, since: None }),
                Value::Object(map) => {
                    let query = map
                        .get("query")
                        .and_then(Value::as_str)
                        .context("query object needs a string \"query\"")?
                        .to_owned();
                    let since = match map.get("since") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(s)) if s.is_empty() => None,
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(other) => {
                            bail!("query {query:?}: since must be a string, got {other}")
                        }
                    };
                    Ok(Query { query, since })
                }
                other => bail!("query entries must be strings or objects, got {other}"),
            })
            .collect::<Result<_>>()?
    } else {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| Query {
                query: l.to_owned(),
                since: None,
            })
            .collect()
    };
    ensure!(!queries.is_empty(), "no queries");
    let mut seen = std::collections::HashSet::new();
    for q in &queries {
        ensure!(seen.insert(q.key()), "duplicate query {:?}", q.key());
    }
    Ok(queries)
}

/// Vault-relative `/`-separated path for refs inside `vault` (mirrors py_recall.normalize_ref).
fn normalize_ref(reference: &str, vault: &str) -> String {
    let prefix = vault.trim_end_matches(['/', '\\']);
    if !prefix.is_empty() {
        if let Some(rest) = reference.strip_prefix(prefix) {
            if let Some(rel) = rest.strip_prefix(['/', '\\']) {
                return rel.replace('\\', "/");
            }
        }
    }
    reference.to_owned()
}

fn normalize_citations(citations: &[lf_ask::Citation], vault: &str) -> Value {
    Value::Array(
        citations
            .iter()
            .map(|c| {
                json!({
                    "n": c.n,
                    "source": c.source,
                    "title": c.title,
                    "app": c.app,
                    "when": c.when,
                    "snippet": c.snippet,
                    "ref": normalize_ref(&c.reference, vault),
                })
            })
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(target_os = "macos")]
fn peak_rss_bytes() -> Option<u64> {
    #[repr(C)]
    struct Timeval {
        tv_sec: i64,
        tv_usec: i32,
    }
    #[repr(C)]
    struct Rusage {
        ru_utime: Timeval,
        ru_stime: Timeval,
        ru_maxrss: i64, // bytes on macOS
        rest: [i64; 13],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    let mut usage = std::mem::MaybeUninit::<Rusage>::zeroed();
    // SAFETY: getrusage(RUSAGE_SELF = 0) fills a caller-owned struct with the libc layout above.
    let ok = unsafe { getrusage(0, usage.as_mut_ptr()) } == 0;
    // SAFETY: zero-initialized and (on success) written by getrusage.
    let max = unsafe { usage.assume_init() }.ru_maxrss;
    (ok && max > 0).then_some(max as u64)
}

#[cfg(windows)]
fn peak_rss_bytes() -> Option<u64> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: the pseudo-handle needs no closing; `counters` is a valid, sized out-struct.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.peak_working_set_size as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

fn main() -> Result<()> {
    let args = parse_args(env::args().skip(1))?;
    let queries = match &args.queries {
        Some(path) => Some(parse_queries(
            &fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        )?),
        None => None,
    };
    let store = HistoryStore::open(Some(args.db.clone()), None, true)?;
    let corpus = Corpus {
        store: Some(&store),
        vault_path: args.vault.clone(),
        notes_folder: "Notes".into(),
        logs_folder: "Transcripts".into(),
    };
    let now = parse_now(&args.now)?;
    let Some(queries) = queries else {
        let options = SearchOptions {
            now,
            ..Default::default()
        };
        for _ in 0..args.warmup {
            search(LEGACY_QUERY, &corpus, &options)?;
        }
        let mut milliseconds = Vec::new();
        let mut citations = Vec::new();
        for _ in 0..args.iterations {
            let start = Instant::now();
            let result = search(LEGACY_QUERY, &corpus, &options)?;
            milliseconds.push(start.elapsed().as_secs_f64() * 1000.0);
            citations = result.citations;
        }
        println!(
            "{}",
            json!({"milliseconds": milliseconds, "citations": citations})
        );
        return Ok(());
    };

    let vault = args.vault.to_string_lossy().into_owned();
    let mut keys = Vec::new();
    let mut milliseconds = Map::new();
    let mut all_milliseconds = Vec::new();
    let mut citations_by_query = Map::new();
    for q in &queries {
        let options = SearchOptions {
            now,
            since: q.since.as_deref().map(Since::from),
            ..Default::default()
        };
        let mut result = None;
        for _ in 0..args.warmup {
            result = Some(search(&q.query, &corpus, &options)?);
        }
        let mut timed = Vec::with_capacity(args.iterations);
        for _ in 0..args.iterations {
            let start = Instant::now();
            let r = search(&q.query, &corpus, &options)?;
            timed.push(start.elapsed().as_secs_f64() * 1000.0);
            result = Some(r);
        }
        let key = q.key();
        all_milliseconds.extend_from_slice(&timed);
        milliseconds.insert(key.clone(), json!(timed));
        let citations = result.map(|r| r.citations).unwrap_or_default();
        citations_by_query.insert(key.clone(), normalize_citations(&citations, &vault));
        keys.push(key);
    }
    println!(
        "{}",
        json!({
            "runtime": "rust",
            "now": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "iterations": args.iterations,
            "warmup": args.warmup,
            "queries": keys,
            "milliseconds": milliseconds,
            "all_milliseconds": all_milliseconds,
            "citations_by_query": citations_by_query,
            "peak_rss_bytes": peak_rss_bytes(),
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn legacy_positional_arguments() {
        let a = args(&["db.sqlite", "vault"]).unwrap();
        assert_eq!(
            a,
            Args {
                db: "db.sqlite".into(),
                vault: "vault".into(),
                queries: None,
                iterations: 1,
                warmup: 0,
                now: DEFAULT_NOW.into(),
            }
        );
        assert_eq!(args(&["db", "vault", "7"]).unwrap().iterations, 7);
    }

    #[test]
    fn legacy_rejects_bad_input() {
        assert!(args(&[]).is_err());
        assert!(args(&["db"]).is_err());
        assert!(args(&["db", "vault", "0"]).is_err());
        assert!(args(&["db", "vault", "x"]).is_err());
        assert!(args(&["db", "vault", "3", "extra"]).is_err());
    }

    #[test]
    fn query_mode_defaults_and_flags() {
        let a = args(&["db", "vault", "--queries", "q.json"]).unwrap();
        assert_eq!(a.queries, Some(PathBuf::from("q.json")));
        assert_eq!((a.iterations, a.warmup), (5, 1));
        let a = args(&[
            "--queries=q.txt",
            "db",
            "--warmup",
            "0",
            "vault",
            "--iterations",
            "3",
            "--now",
            "2026-01-02T03:04:05",
        ])
        .unwrap();
        assert_eq!((a.iterations, a.warmup), (3, 0));
        assert_eq!(a.now, "2026-01-02T03:04:05");
        assert_eq!(a.db, PathBuf::from("db"));
        assert_eq!(a.vault, PathBuf::from("vault"));
        // Positional ITERATIONS still works with --queries.
        assert_eq!(
            args(&["db", "vault", "4", "--queries", "q"])
                .unwrap()
                .iterations,
            4
        );
    }

    #[test]
    fn query_mode_rejects_bad_flags() {
        assert!(args(&["db", "vault", "--queries"]).is_err());
        assert!(args(&["db", "vault", "--bogus", "1"]).is_err());
        assert!(args(&["db", "vault", "--iterations", "0", "--queries", "q"]).is_err());
        assert!(args(&["db", "vault", "--warmup", "-1"]).is_err());
        assert!(args(&["db", "vault", "--now", "yesterday"]).is_err());
        assert!(args(&["db", "vault", "2", "--iterations", "3"]).is_err());
    }

    #[test]
    fn parses_json_query_list() {
        let q = parse_queries(
            r#"["launch pricing", {"query": "budget", "since": "7d"}, {"query": "roadmap", "since": null}]"#,
        )
        .unwrap();
        assert_eq!(
            q,
            vec![
                Query {
                    query: "launch pricing".into(),
                    since: None
                },
                Query {
                    query: "budget".into(),
                    since: Some("7d".into())
                },
                Query {
                    query: "roadmap".into(),
                    since: None
                },
            ]
        );
        assert_eq!(q[1].key(), "budget (since 7d)");
        assert_eq!(q[0].key(), "launch pricing");
    }

    #[test]
    fn parses_newline_query_file() {
        let q = parse_queries("\u{feff}budget\n\n  zephyr  \r\n\"quoted phrase\"\n").unwrap();
        let keys: Vec<_> = q.iter().map(Query::key).collect();
        assert_eq!(keys, ["budget", "zephyr", "\"quoted phrase\""]);
    }

    #[test]
    fn rejects_bad_query_files() {
        assert!(parse_queries("").is_err());
        assert!(parse_queries("[]").is_err());
        assert!(parse_queries("[1]").is_err());
        assert!(parse_queries(r#"[{"since": "7d"}]"#).is_err());
        assert!(parse_queries(r#"[{"query": "a", "since": 7}]"#).is_err());
        assert!(parse_queries(r#"["a", "a"]"#).is_err());
        assert!(parse_queries("[not json").is_err());
    }

    #[test]
    fn normalizes_vault_refs() {
        assert_eq!(
            normalize_ref(
                "/tmp/c/vault/Transcripts/Zoom/x (transcript).md#01:02",
                "/tmp/c/vault"
            ),
            "Transcripts/Zoom/x (transcript).md#01:02"
        );
        assert_eq!(
            normalize_ref("/tmp/c/vault/Notes/a.md", "/tmp/c/vault/"),
            "Notes/a.md"
        );
        assert_eq!(
            normalize_ref(r"C:\c\vault\Notes\a.md", r"C:\c\vault"),
            "Notes/a.md"
        );
        assert_eq!(normalize_ref("42", "/tmp/c/vault"), "42");
        assert_eq!(
            normalize_ref("/tmp/c/vault2/Notes/a.md", "/tmp/c/vault"),
            "/tmp/c/vault2/Notes/a.md"
        );
    }

    #[test]
    fn peak_rss_is_positive_when_available() {
        if let Some(bytes) = peak_rss_bytes() {
            assert!(bytes > 0);
        }
    }
}
