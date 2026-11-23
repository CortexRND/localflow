use crate::text::{ascii_digits, python_whitespace};
use crate::{extract_phrases, tokenize, Passage};
use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, Offset, TimeZone};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

pub const BM25_K1: f64 = 1.5;
pub const BM25_B: f64 = 0.75;
pub const PHRASE_BOOST: f64 = 3.0;
pub const RECENCY_WEIGHT: f64 = 0.1;
pub const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;
static RELATIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)[\s\x1c-\x1f]*d$").unwrap());
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-[0-9]{1,2}-[0-9]{1,2}$").unwrap());

#[derive(Clone, Debug)]
pub enum Since {
    Text(String),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
}
impl From<&str> for Since {
    fn from(s: &str) -> Self {
        Self::Text(s.into())
    }
}
impl From<NaiveDate> for Since {
    fn from(d: NaiveDate) -> Self {
        Self::Date(d)
    }
}
impl From<NaiveDateTime> for Since {
    fn from(d: NaiveDateTime) -> Self {
        Self::DateTime(d)
    }
}
pub fn parse_since(value: Option<&Since>, now: NaiveDateTime) -> Result<Option<NaiveDateTime>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = match value {
        Since::DateTime(d) => return Ok(Some(*d)),
        Since::Date(d) => return Ok(d.and_hms_opt(0, 0, 0)),
        Since::Text(s) => s,
    };
    if text.is_empty() {
        return Ok(None);
    }
    let normalized = text.trim_matches(python_whitespace).to_lowercase();
    let midnight = now.date().and_hms_opt(0, 0, 0).unwrap();
    let invalid = || {
        anyhow!(
            "invalid since value {}; use today, yesterday, 7d, 30d or YYYY-MM-DD",
            crate::text::repr(text)
        )
    };
    let parsed = match normalized.as_str() {
        "today" => Some(midnight),
        "yesterday" => midnight.checked_sub_signed(Duration::days(1)),
        s => {
            if let Some(c) = RELATIVE.captures(s) {
                let days = ascii_digits(&c[1]).parse::<i64>().map_err(|_| invalid())?;
                Duration::try_days(days).and_then(|d| now.checked_sub_signed(d))
            } else {
                NaiveDate::parse_from_str(&ascii_digits(s), "%Y-%m-%d")
                    .ok()
                    .filter(|d| DATE.is_match(s) && d.year() >= 1)
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
            }
        }
    };
    Ok(Some(parsed.ok_or_else(invalid)?))
}
/// Whether a recall date bound is relative to the current time, not a calendar day.
pub fn is_relative_since(text: &str) -> bool {
    RELATIVE.is_match(&text.trim_matches(python_whitespace).to_lowercase())
}

/// Convert a naive local datetime to a Unix timestamp using Python's DST rules.
pub fn timestamp(local: NaiveDateTime) -> f64 {
    if let Some(dt) = Local.from_local_datetime(&local).earliest() {
        return dt.timestamp() as f64 + dt.timestamp_subsec_nanos() as f64 / 1e9;
    }
    // Python's naive timestamp() maps a skipped local hour with the pre-transition offset.
    let offset = (1..=48)
        .find_map(|h| {
            local
                .checked_sub_signed(Duration::hours(h))
                .and_then(|d| Local.from_local_datetime(&d).earliest())
        })
        .map(|d| d.offset().fix().local_minus_utc())
        .unwrap_or(0);
    local.and_utc().timestamp() as f64 + local.and_utc().timestamp_subsec_nanos() as f64 / 1e9
        - offset as f64
}
#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub k: usize,
    pub since: Option<Since>,
    pub now: NaiveDateTime,
    /// Empty means all sources. This is an additive filter over Python recall.
    pub sources: Vec<String>,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            k: 8,
            since: None,
            now: Local::now().naive_local(),
            sources: vec![],
        }
    }
}
pub fn rank(question: &str, passages: &[Passage], options: &SearchOptions) -> Result<Vec<Passage>> {
    let floor = parse_since(options.since.as_ref(), options.now)?.map(timestamp);
    let passages: Vec<_> = passages
        .iter()
        .filter(|p| {
            floor.is_none_or(|f| p.ts.is_some_and(|ts| ts >= f))
                && (options.sources.is_empty() || options.sources.contains(&p.source))
        })
        .collect();
    let (plain, phrases) = extract_phrases(question);
    let mut seen = HashSet::new();
    let terms: Vec<_> = tokenize(&plain, false)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect();
    if terms.is_empty() || passages.is_empty() || options.k == 0 {
        return Ok(vec![]);
    }
    let docs: Vec<_> = passages.iter().map(|p| tokenize(&p.text, false)).collect();
    let n = docs.len() as f64;
    let avg_len = (docs.iter().map(Vec::len).sum::<usize>() as f64 / n).max(f64::MIN_POSITIVE);
    let avg_len = if docs.iter().all(Vec::is_empty) {
        1.0
    } else {
        avg_len
    };
    let mut df: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        for term in doc.iter().map(String::as_str).collect::<HashSet<_>>() {
            if seen.contains(term) {
                *df.entry(term).or_default() += 1;
            }
        }
    }
    let idf: Vec<_> = terms
        .iter()
        .filter_map(|t| {
            df.get(t.as_str()).map(|&freq| {
                (
                    t,
                    (1.0 + (n - freq as f64 + 0.5) / (freq as f64 + 0.5)).ln(),
                )
            })
        })
        .collect();
    let mut scored = Vec::new();
    let now = timestamp(options.now);
    for (index, (passage, doc)) in passages.iter().zip(&docs).enumerate() {
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for term in doc {
            *tf.entry(term).or_default() += 1;
        }
        let mut score = 0.0;
        for (term, weight) in &idf {
            let freq = *tf.get(term.as_str()).unwrap_or(&0) as f64;
            if freq > 0.0 {
                let norm = freq + BM25_K1 * (1.0 - BM25_B + BM25_B * doc.len() as f64 / avg_len);
                score += weight * freq * (BM25_K1 + 1.0) / norm;
            }
        }
        if score <= 0.0 {
            continue;
        }
        if !phrases.is_empty() {
            let full = tokenize(&passage.text, true);
            score += PHRASE_BOOST
                * phrases
                    .iter()
                    .filter(|ph| full.windows(ph.len()).any(|w| w == ph.as_slice()))
                    .count() as f64;
        }
        if let Some(ts) = passage.ts {
            let age_days = ((now - ts) / 86400.0).max(0.0);
            score *= 1.0 + RECENCY_WEIGHT * 0.5_f64.powf(age_days / RECENCY_HALF_LIFE_DAYS);
        }
        scored.push((
            score,
            passage.ts.filter(|ts| *ts != 0.0).unwrap_or(0.0),
            index,
        ));
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    Ok(scored
        .into_iter()
        .take(options.k)
        .map(|(_, _, i)| passages[i].clone())
        .collect())
}
