//! `recall`: the Python `lf recall` over `lf_ask` (dictation history, meeting
//! transcripts and notes in the vault), answered by the configured LLM unless
//! `--no-llm`.

use std::cell::RefCell;

use anyhow::{anyhow, Result};
use chrono::{Local, NaiveDateTime};
use lf_ask::{Citation, Corpus, SearchOptions, Since, NO_ANSWER};
use lf_core::symbols::pycompat;
use lf_core::{config, history::HistoryStore};
use lf_providers::factory::{build_llm, BuildOptions};
use lf_providers::{CoreSecrets, Health, LlmProvider, ModelInfo, ProviderError};

use crate::args::RecallArgs;
use crate::pyfmt::{py_space, py_strip};
use crate::richfmt::Console;
use crate::{outln, usage, Io};

pub(crate) fn run(args: RecallArgs, io: &mut Io<'_>) -> Result<()> {
    // click processes the given `--k` before the missing QUESTION.
    let k = parse_k(&args.k)?;
    let Some(question) = args.question else {
        return usage("Missing argument 'QUESTION'.");
    };
    if py_strip(&question).is_empty() {
        return usage("question is empty");
    }
    let since = args.since.as_deref().map(Since::from);
    if let Err(error) = lf_ask::parse_since(since.as_ref(), Local::now().naive_local()) {
        return usage(format!("Invalid value for --since: {error}"));
    }
    let loaded = config::load_config()?;
    let store = if loaded.field_bool("history_enabled") == Some(false) {
        None
    } else {
        Some(HistoryStore::open(None, None, true)?)
    };
    let corpus = Corpus::from_config(&loaded, store.as_ref());
    let options = SearchOptions {
        k,
        since,
        now: Local::now().naive_local(),
        sources: Vec::new(),
    };
    let result = if args.no_llm {
        lf_ask::search(&question, &corpus, &options)?
    } else {
        // `console.status(...)` draws nothing when stdout is not a terminal.
        let llm = Recording {
            inner: build_llm(&loaded, &BuildOptions::default(), &CoreSecrets)
                .map_err(|error| anyhow!("{error}"))?,
            error: RefCell::new(None),
        };
        let result = lf_ask::recall(&question, &llm, &corpus, &options)?;
        if let Some(error) = llm.error.take() {
            // Python's `log.warning` reaches stderr through logging's last resort.
            outln!(
                io.err,
                "recall: LLM unavailable, returning passages only: {error}"
            )?;
        }
        result
    };
    if args.json {
        return outln!(
            io.out,
            "{}",
            pycompat::json_dumps(&result.to_dict(), Some(2), false)
        );
    }
    let console = Console::detect();
    let answer = result.answer.as_deref().filter(|answer| !answer.is_empty());
    if let Some(answer) = answer {
        write!(io.out, "{}", console.text(answer))?;
    } else if !args.no_llm {
        write!(
            io.out,
            "{}",
            console.markup("[yellow]LLM unavailable — showing matching passages only.[/yellow]")
        )?;
    }
    if result.citations.is_empty() {
        if answer.is_none() {
            write!(io.out, "{}", console.markup(NO_ANSWER))?;
        }
        return Ok(());
    }
    write!(io.out, "{}", console.markup("\n[bold]Sources[/bold]"))?;
    for citation in &result.citations {
        write!(io.out, "{}", console.text(&format_source(citation)))?;
    }
    Ok(())
}

/// click's `IntRange(min=1, max=20)` for `--k`.
fn parse_k(raw: &str) -> Result<usize> {
    let text = raw.trim_matches(py_space);
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let value = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse::<i64>().ok())
        .flatten();
    match value {
        Some(value) if (1..=20).contains(&value) => Ok(value as usize),
        Some(value) => usage(format!(
            "Invalid value for '--k': {value} is not in the range 1<=x<=20."
        )),
        None => usage(format!(
            "Invalid value for '--k': {} is not a valid integer range.",
            crate::pyfmt::str_repr(raw)
        )),
    }
}

/// `cli._format_recall_source`.
fn format_source(citation: &Citation) -> String {
    let when = NaiveDateTime::parse_from_str(&citation.when, "%Y-%m-%dT%H:%M")
        .map(|when| format!("{} · ", when.format("%a %b %d %H:%M")))
        .unwrap_or_default();
    let mut label = citation.source.clone();
    if !citation.title.is_empty() {
        label.push_str(" · ");
        label.push_str(&citation.title);
    }
    format!("[{}] {when}{label} — \"{}\"", citation.n, citation.snippet)
}

/// Keeps the provider error `lf_ask::recall` swallows, so the warning Python
/// logs for "LLM unavailable" can be printed.
struct Recording {
    inner: Box<dyn LlmProvider>,
    error: RefCell<Option<String>>,
}

impl LlmProvider for Recording {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn offline(&self) -> bool {
        self.inner.offline()
    }

    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError> {
        let result = self.inner.complete(system, user, max_tokens);
        if let Err(error) = &result {
            *self.error.borrow_mut() = Some(error.0.clone());
        }
        result
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }

    fn healthcheck(&self) -> Health {
        self.inner.healthcheck()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_follows_click_int_range() {
        assert_eq!(parse_k("8").unwrap(), 8);
        assert_eq!(parse_k(" +20 ").unwrap(), 20);
        for (raw, message) in [
            (
                "0",
                "Invalid value for '--k': 0 is not in the range 1<=x<=20.",
            ),
            (
                "-3",
                "Invalid value for '--k': -3 is not in the range 1<=x<=20.",
            ),
            (
                "abc",
                "Invalid value for '--k': 'abc' is not a valid integer range.",
            ),
            (
                "",
                "Invalid value for '--k': '' is not a valid integer range.",
            ),
        ] {
            assert_eq!(parse_k(raw).unwrap_err().to_string(), message, "{raw}");
        }
    }

    #[test]
    fn sources_use_python_strftime() {
        let citation = Citation {
            n: 2,
            source: "meeting".into(),
            title: "Launch".into(),
            app: String::new(),
            when: "2026-11-13T09:05".into(),
            snippet: "launch pricing".into(),
            reference: String::new(),
        };
        assert_eq!(
            format_source(&citation),
            "[2] Fri Nov 13 09:05 · meeting · Launch — \"launch pricing\""
        );
        let bare = Citation {
            when: String::new(),
            title: String::new(),
            ..citation
        };
        assert_eq!(format_source(&bare), "[2] meeting — \"launch pricing\"");
    }
}
