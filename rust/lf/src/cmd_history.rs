//! `history [list|search|copy|raw|delete|clear|stats]` and the hidden
//! top-level `stats` (localflow/cli.py `history` group and `stats`).

use anyhow::Result;
use chrono::{Local, TimeZone};
use lf_core::config;
use lf_core::history::{HistoryEntry, HistoryStore};

use crate::args::{HistoryArgs, HistoryCommand};
use crate::cmd_config::parse_py_int;
use crate::pyfmt;
use crate::{abort, confirm, fail, outln, Io};

/// `HistoryStore(enabled=load_config().history_enabled)`.
fn open_store() -> Result<HistoryStore> {
    let loaded = config::load_config()?;
    let enabled = loaded
        .fields
        .get("history_enabled")
        .map(truthy)
        .unwrap_or(true);
    HistoryStore::open(None, None, enabled)
}

/// Python truthiness of a config value.
fn truthy(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

pub(crate) fn run(args: HistoryArgs, io: &mut Io<'_>) -> Result<()> {
    let store = open_store()?;
    run_with(&store, args, io)
}

pub(crate) fn stats(io: &mut Io<'_>) -> Result<()> {
    show_stats(&open_store()?, io)
}

/// The command against an explicit store (unit tests use a temp database).
pub(crate) fn run_with(store: &HistoryStore, args: HistoryArgs, io: &mut Io<'_>) -> Result<()> {
    match args.command {
        None => show(
            &store.recent(args.limit.into(), args.search.as_deref(), 0)?,
            io,
        ),
        Some(HistoryCommand::List { limit }) => show(&store.recent(limit.into(), None, 0)?, io),
        Some(HistoryCommand::Search { query, limit }) => {
            show(&store.recent(limit.into(), Some(&query), 0)?, io)
        }
        Some(HistoryCommand::Copy { entry_id }) => {
            let entry = resolve(store, &entry_id)?;
            copy(&entry.final_text, io)?;
            outln!(io.out, "Copied final text.")
        }
        Some(HistoryCommand::Raw { entry_id }) => {
            let entry = resolve(store, &entry_id)?;
            copy(&entry.raw_text, io)?;
            outln!(io.out, "Copied raw transcript.")
        }
        Some(HistoryCommand::Delete { entry_id }) => {
            let entry = resolve(store, &entry_id)?;
            if !store.delete(entry.id)? {
                return fail(format!("history entry {entry_id} not found"));
            }
            outln!(io.out, "Deleted history entry {}.", entry.id)
        }
        Some(HistoryCommand::Clear { yes }) => {
            if !yes && !confirm("Delete all history entries and stored clips?", io)? {
                return abort(io);
            }
            let count = store.clear()?;
            outln!(io.out, "Cleared {count} history entries.")
        }
        Some(HistoryCommand::Stats) => show_stats(store, io),
    }
}

fn copy(text: &str, io: &mut Io<'_>) -> Result<()> {
    io.clipboard
        .set_text(text)
        .or_else(|error| fail(format!("{error:#}")))
}

/// `cli._history_entry`: "last" or an integer id (Python `int()` syntax).
fn resolve(store: &HistoryStore, entry_id: &str) -> Result<HistoryEntry> {
    let entry = if entry_id == "last" {
        store.recent(1, None, 0)?.into_iter().next()
    } else {
        match parse_py_int(entry_id) {
            Some(id) => store.get(id)?,
            None => {
                return fail(format!(
                    "invalid history entry {}",
                    pyfmt::str_repr(entry_id)
                ))
            }
        }
    };
    match entry {
        Some(entry) => Ok(entry),
        None => fail(format!("history entry {entry_id} not found")),
    }
}

/// `datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M")` in local time.
pub(crate) fn when(ts: f64) -> String {
    if !ts.is_finite() {
        return String::new();
    }
    let micros = (ts * 1_000_000.0).round();
    if micros.abs() > i64::MAX as f64 {
        return String::new();
    }
    let micros = micros as i64;
    match Local.timestamp_micros(micros).single() {
        Some(moment) => moment.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

/// The table `lf history` prints (plain text, columns padded to width).
pub(crate) fn render_table(entries: &[HistoryEntry]) -> Vec<String> {
    let header = ["ID", "When", "Kind", "App", "Status", "Text"].map(String::from);
    let mut rows = vec![header.to_vec()];
    for entry in entries {
        let text = if !entry.final_text.is_empty() {
            &entry.final_text
        } else {
            &entry.raw_text
        };
        let text: String = text.replace('\n', " ").chars().take(80).collect();
        let app = match entry.app_name.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => "Unknown".to_string(),
        };
        rows.push(vec![
            entry.id.to_string(),
            when(entry.ts),
            entry.kind.clone(),
            app,
            entry.status.clone(),
            text,
        ]);
    }
    let columns = rows[0].len();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (column, cell) in row.iter().enumerate() {
                if column + 1 == columns {
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    let pad = widths[column] - cell.chars().count() + 2;
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn show(entries: &[HistoryEntry], io: &mut Io<'_>) -> Result<()> {
    if entries.is_empty() {
        return outln!(io.out, "No history entries.");
    }
    for line in render_table(entries) {
        outln!(io.out, "{line}")?;
    }
    Ok(())
}

fn show_stats(store: &HistoryStore, io: &mut Io<'_>) -> Result<()> {
    let stats = store.stats(None)?;
    outln!(io.out, "Dictations: {}", stats.total_dictations)?;
    outln!(io.out, "Words: {}", stats.total_words)?;
    outln!(io.out, "Average WPM: {:.1}", stats.avg_wpm)?;
    outln!(io.out, "Words today: {}", stats.words_today)?;
    outln!(io.out, "Words this week: {}", stats.words_7d)?;
    outln!(io.out, "Current streak: {} days", stats.current_streak_days)?;
    outln!(io.out, "Longest streak: {} days", stats.longest_streak_days)?;
    if !stats.top_apps.is_empty() {
        let apps: Vec<String> = stats
            .top_apps
            .iter()
            .map(|(name, count)| format!("{name} ({count})"))
            .collect();
        outln!(io.out, "Top apps: {}", apps.join(", "))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeClipboard;
    use crate::CliError;
    use lf_core::history::NewEntry;
    use std::io::Cursor;

    struct Run {
        code: i32,
        out: String,
        err: String,
    }

    fn run_history(
        store: &HistoryStore,
        command: Option<HistoryCommand>,
        input: &str,
        clipboard: &mut FakeClipboard,
    ) -> Run {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut reader,
            clipboard,
            server_url: None,
            terminal: false,
        };
        let args = HistoryArgs {
            limit: 50,
            search: None,
            command,
        };
        let code = match run_with(store, args, &mut io) {
            Ok(()) => 0,
            Err(error) => match error.downcast_ref::<CliError>() {
                Some(CliError::Exit(code)) => *code,
                Some(CliError::Failed(message)) => {
                    let _ = write!(io.err, "Error: {message}");
                    1
                }
                _ => 99,
            },
        };
        Run {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn store_with_entries() -> (tempfile::TempDir, HistoryStore, i64, i64) {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let mut first = NewEntry::new("dictate");
        first.raw_text = "Raw words".into();
        first.final_text = "Clean words".into();
        first.app_name = Some("Editor".into());
        first.ts = Some(1.0);
        let first = store.add(&first).unwrap().unwrap();
        let mut second = NewEntry::new("command");
        second.raw_text = "only raw\ntext".into();
        second.ts = Some(2.0);
        let second = store.add(&second).unwrap().unwrap();
        (dir, store, first, second)
    }

    #[test]
    fn empty_history_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(Some(dir.path().join("h.sqlite")), None, true).unwrap();
        let mut clipboard = FakeClipboard::default();
        assert_eq!(
            run_history(&store, None, "", &mut clipboard).out,
            "No history entries.\n"
        );
        let copy = run_history(
            &store,
            Some(HistoryCommand::Copy {
                entry_id: "last".into(),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(
            (copy.code, copy.err.as_str()),
            (1, "Error: history entry last not found")
        );
        let stats = run_history(&store, Some(HistoryCommand::Stats), "", &mut clipboard);
        assert_eq!(
            stats.out,
            "Dictations: 0\nWords: 0\nAverage WPM: 0.0\nWords today: 0\n\
             Words this week: 0\nCurrent streak: 0 days\nLongest streak: 0 days\n"
        );
    }

    #[test]
    fn list_copy_raw_and_delete() {
        let (_dir, store, first, second) = store_with_entries();
        let mut clipboard = FakeClipboard::default();
        let listed = run_history(&store, None, "", &mut clipboard);
        let lines: Vec<&str> = listed.out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("ID"));
        assert!(lines[1].contains("only raw text") && lines[1].contains("Unknown"));
        assert!(lines[2].contains("Clean words") && lines[2].contains("Editor"));

        let copy = run_history(
            &store,
            Some(HistoryCommand::Copy {
                entry_id: first.to_string(),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(copy.out, "Copied final text.\n");
        let raw = run_history(
            &store,
            Some(HistoryCommand::Raw {
                entry_id: "last".into(),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(raw.out, "Copied raw transcript.\n");
        assert_eq!(clipboard.copied, ["Clean words", "only raw\ntext"]);

        let bad = run_history(
            &store,
            Some(HistoryCommand::Delete {
                entry_id: "abc".into(),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(
            (bad.code, bad.err.as_str()),
            (1, "Error: invalid history entry 'abc'")
        );
        let deleted = run_history(
            &store,
            Some(HistoryCommand::Delete {
                entry_id: format!(" {second} "),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(deleted.out, format!("Deleted history entry {second}.\n"));
        assert!(store.get(second).unwrap().is_none());
    }

    #[test]
    fn copy_reports_an_unavailable_clipboard() {
        let (_dir, store, first, _) = store_with_entries();
        let mut clipboard = FakeClipboard {
            unavailable: true,
            ..Default::default()
        };
        let copy = run_history(
            &store,
            Some(HistoryCommand::Copy {
                entry_id: first.to_string(),
            }),
            "",
            &mut clipboard,
        );
        assert_eq!(copy.code, 1);
        assert!(copy.out.is_empty());
        assert!(copy.err.contains("clipboard unavailable"));
    }

    #[test]
    fn clear_requires_confirmation() {
        let (_dir, store, _, _) = store_with_entries();
        let mut clipboard = FakeClipboard::default();
        let clear = || Some(HistoryCommand::Clear { yes: false });
        for input in ["n\n", "\n", "", "maybe\nno\n"] {
            let refused = run_history(&store, clear(), input, &mut clipboard);
            assert_eq!(refused.code, 1, "{input:?}");
            assert!(refused.err.contains("Aborted!"));
            assert_eq!(store.recent(10, None, 0).unwrap().len(), 2);
        }
        let accepted = run_history(&store, clear(), "Yes\n", &mut clipboard);
        assert_eq!(
            accepted.out,
            "Delete all history entries and stored clips? [y/N]: Cleared 2 history entries.\n"
        );
        let again = run_history(
            &store,
            Some(HistoryCommand::Clear { yes: true }),
            "",
            &mut clipboard,
        );
        assert_eq!(again.out, "Cleared 0 history entries.\n");
    }

    #[test]
    fn table_truncates_and_flattens_text() {
        let entry = HistoryEntry {
            id: 7,
            ts: 0.0,
            kind: "dictate".into(),
            app_name: Some(String::new()),
            bundle_id: None,
            category: None,
            duration_s: 0.0,
            raw_text: String::new(),
            final_text: format!("a\nb{}", "x".repeat(100)),
            words: 1,
            status: "ok".into(),
            audio_path: None,
        };
        let lines = render_table(&[entry]);
        assert!(lines[1].ends_with(&format!("a b{}", "x".repeat(77))));
        assert!(lines[1].contains("Unknown"));
        assert_eq!(when(f64::NAN), "");
    }
}
