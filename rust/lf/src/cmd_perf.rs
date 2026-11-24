//! `perf [--window 24h|7d|30d] [--json] [--clear]` (localflow/cli.py
//! `perf_command`): the local perf ledger summary, read straight from the
//! ledger file (no server needed). The summary, its JSON and its table come
//! from `lf_core::perf_ledger`, so the output matches Python's byte for byte.

use anyhow::Result;
use lf_core::perf_ledger::{self, PerfLedger, WINDOWS};
use lf_core::symbols::pycompat;

use crate::args::PerfArgs;
use crate::pyfmt::str_repr;
use crate::{fail, outln, usage, Io};

pub(crate) fn run(args: PerfArgs, io: &mut Io<'_>) -> Result<()> {
    run_with(&PerfLedger::default(), args, io)
}

/// The command against an explicit ledger (unit tests use a temp file).
pub(crate) fn run_with(ledger: &PerfLedger, args: PerfArgs, io: &mut Io<'_>) -> Result<()> {
    // click validates the `Choice` before the command runs, `--clear` included.
    if perf_ledger::parse_window(&args.window).is_err() {
        let choices: Vec<String> = WINDOWS.iter().map(|(name, _)| str_repr(name)).collect();
        return usage(format!(
            "Invalid value for '--window': {} is not one of {}.",
            str_repr(&args.window),
            choices.join(", ")
        ));
    }
    if args.clear {
        if let Err(error) = ledger.clear() {
            return fail(format!("could not clear the perf ledger: {error}"));
        }
        return outln!(io.out, "Cleared the perf ledger.");
    }
    let summary = perf_ledger::summarize(ledger, &args.window)?;
    if args.json {
        // Python `json.dumps(summary, indent=2)`.
        return outln!(io.out, "{}", pycompat::json_dumps(&summary, Some(2), true));
    }
    for line in perf_ledger::render_summary(&summary) {
        outln!(io.out, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::testing::{run_cli, ENV_LOCK};
    use lf_core::perf_ledger::{Kind, PerfEvent, PerfLedger, Status, SttLabels, ENV_PATH};
    use serde_json::Value;

    /// Points `LOCALFLOW_PERF_LEDGER` at a temp file while held.
    struct Ledger {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        ledger: PerfLedger,
    }

    impl Ledger {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("perf").join("events.jsonl");
            std::env::set_var(ENV_PATH, &path);
            Self {
                _lock: lock,
                _dir: dir,
                ledger: PerfLedger::new(path),
            }
        }
    }

    impl Drop for Ledger {
        fn drop(&mut self) {
            std::env::remove_var(ENV_PATH);
        }
    }

    fn record(ledger: &PerfLedger, kind: Kind, status: Status, total: f64) {
        assert!(ledger.record(&PerfEvent {
            audio_s: Some(2.0),
            stt_ms: Some(total / 2.0),
            total_ms: Some(total),
            stt: Some(SttLabels::new("faster-whisper", "base", "cpu")),
            ..PerfEvent::new(kind, status)
        }));
    }

    #[test]
    fn empty_state_per_window() {
        let fixture = Ledger::new();
        for (args, window) in [(&["perf"][..], "7d"), (&["perf", "--window", "24h"], "24h")] {
            let outcome = run_cli(args);
            assert_eq!(
                (outcome.code, outcome.out, outcome.err),
                (
                    0,
                    format!("No perf events in the last {window}.\n"),
                    String::new()
                )
            );
        }
        assert!(!fixture.ledger.path().exists());
    }

    #[test]
    fn table_and_json() {
        let fixture = Ledger::new();
        record(&fixture.ledger, Kind::Dictate, Status::Ok, 100.0);
        record(&fixture.ledger, Kind::Dictate, Status::Ok, 300.0);
        record(&fixture.ledger, Kind::Dictate, Status::Error, 1.0);
        record(&fixture.ledger, Kind::SttLoad, Status::Ok, 640.0);
        let table = run_cli(&["perf"]);
        assert_eq!(table.code, 0, "{}", table.err);
        let lines: Vec<&str> = table.out.lines().collect();
        assert_eq!(lines[0], "Perf events in the last 7d: 4");
        assert_eq!(lines[3], "dictate   3      1       33.3%         0.05");
        assert!(lines.contains(&"dictate   total_ms  200.0   290.0   2"));
        assert!(lines.contains(&"stt_load  faster-whisper  base   1      640.0   640.0   640.0"));
        assert_eq!(
            *lines.last().unwrap(),
            "Dictation: median 200.0 ms, p95 290.0 ms, RTF 0.05, slowest stage stt_ms"
        );

        let raw = run_cli(&["perf", "--json", "--window", "30d"]);
        assert_eq!(raw.code, 0);
        assert!(raw
            .out
            .starts_with("{\n  \"window\": \"30d\",\n  \"window_seconds\": 2592000,\n"));
        let value: Value = serde_json::from_str(&raw.out).unwrap();
        assert_eq!(value["by_kind"]["dictate"]["failure_rate"], 0.3333);
        assert_eq!(value["dictation"]["median_total_ms"], 200.0);
    }

    #[test]
    fn invalid_window_is_a_usage_error_even_with_clear() {
        let fixture = Ledger::new();
        record(&fixture.ledger, Kind::Recall, Status::Ok, 1.0);
        for args in [
            &["perf", "--window", "1y"][..],
            &["perf", "--clear", "--window", "7D"],
        ] {
            let outcome = run_cli(args);
            assert_eq!((outcome.code, outcome.out.as_str()), (2, ""), "{args:?}");
            let bad = args[args.len() - 1];
            assert_eq!(
                outcome.err,
                format!(
                    "Error: Invalid value for '--window': '{bad}' is not one of '24h', '7d', '30d'.\n"
                )
            );
        }
        assert!(fixture.ledger.path().exists());
    }

    #[test]
    fn clear_removes_both_files() {
        let fixture = Ledger::new();
        let ledger = fixture.ledger.clone().with_max_bytes(1);
        record(&ledger, Kind::Dictate, Status::Ok, 1.0);
        record(&ledger, Kind::Dictate, Status::Ok, 2.0);
        assert!(ledger.path().exists() && ledger.previous_path().exists());
        for _ in 0..2 {
            let outcome = run_cli(&["perf", "--clear"]);
            assert_eq!(
                (outcome.code, outcome.out.as_str(), outcome.err.as_str()),
                (0, "Cleared the perf ledger.\n", "")
            );
        }
        assert!(!ledger.path().exists() && !ledger.previous_path().exists());
    }

    #[test]
    fn perf_is_listed_in_help() {
        let help = run_cli(&["--help"]);
        assert!(help
            .out
            .lines()
            .any(|line| line.trim_start().starts_with("perf ")));
        let perf = run_cli(&["perf", "--help"]);
        assert_eq!(perf.code, 0);
        assert!(perf
            .out
            .contains("Show local speed metrics from the perf ledger."));
        for flag in ["--window", "--json", "--clear"] {
            assert!(perf.out.contains(flag), "{flag}");
        }
    }
}
