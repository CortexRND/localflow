//! `dict list|add|remove|import` (localflow/cli.py `dict` group).

use std::path::Path;

use anyhow::Result;
use lf_core::dictionary::DictionaryStore;

use crate::args::DictCommand;
use crate::pyfmt::{self, py_strip};
use crate::{fail, outln, usage, Io};

pub(crate) fn run(command: DictCommand, io: &mut Io<'_>) -> Result<()> {
    let store = DictionaryStore::new(None);
    run_with(&store, command, io)
}

/// The command against an explicit store (unit tests use a temp path).
pub(crate) fn run_with(
    store: &DictionaryStore,
    command: DictCommand,
    io: &mut Io<'_>,
) -> Result<()> {
    match command {
        DictCommand::List => list(store, io),
        DictCommand::Add {
            word,
            source,
            target,
        } => add(store, word, source, target, io),
        DictCommand::Remove { word_or_from } => remove(store, &word_or_from, io),
        DictCommand::Import { file_path } => import(store, &file_path, io),
    }
}

fn list(store: &DictionaryStore, io: &mut Io<'_>) -> Result<()> {
    let entries = store.list();
    if entries.words.is_empty() && entries.replacements.is_empty() {
        return outln!(io.out, "No dictionary entries.");
    }
    for word in &entries.words {
        outln!(io.out, "word: {word}")?;
    }
    for pair in &entries.replacements {
        outln!(io.out, "{} -> {}", pair.from, pair.to)?;
    }
    Ok(())
}

fn add(
    store: &DictionaryStore,
    word: Option<String>,
    source: Option<String>,
    target: Option<String>,
    io: &mut Io<'_>,
) -> Result<()> {
    if word.is_some() && (source.is_some() || target.is_some()) {
        return usage("use a word or --from/--to, not both");
    }
    let (added, label) = match (word, source, target) {
        (Some(word), None, None) => {
            let added = store.add_word(&word).or_else(failed)?;
            (added, format!("word: {}", py_strip(&word)))
        }
        (None, Some(source), Some(target)) => {
            let added = store.add_replacement(&source, &target).or_else(failed)?;
            (
                added,
                format!("{} -> {}", py_strip(&source), py_strip(&target)),
            )
        }
        _ => return usage("provide a word or both --from and --to"),
    };
    let state = if added { "added" } else { "already exists" };
    outln!(io.out, "{state}: {label}")
}

/// Store errors become `Error: <message>` with status 1 (click.ClickException).
fn failed<T>(error: anyhow::Error) -> Result<T> {
    fail(format!("{error:#}"))
}

fn remove(store: &DictionaryStore, value: &str, io: &mut Io<'_>) -> Result<()> {
    if !store.remove(value, None).or_else(failed)? {
        return fail(format!("dictionary entry not found: {value}"));
    }
    outln!(io.out, "removed: {value}")
}

fn import(store: &DictionaryStore, path: &Path, io: &mut Io<'_>) -> Result<()> {
    // click.Path(exists=True, dir_okay=False): a usage error, status 2.
    let shown = pyfmt::str_repr(&path.display().to_string());
    if path.is_dir() {
        return usage(format!(
            "Invalid value for 'FILE_PATH': File {shown} is a directory."
        ));
    }
    if !path.exists() {
        return usage(format!(
            "Invalid value for 'FILE_PATH': File {shown} does not exist."
        ));
    }
    let counts = store.import_file(path).or_else(failed)?;
    outln!(
        io.out,
        "added {}; skipped_duplicate {}; skipped_invalid {}",
        counts.added,
        counts.skipped_duplicate,
        counts.skipped_invalid
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeClipboard, Outcome};
    use crate::CliError;
    use std::io::Cursor;

    fn run_dict(store: &DictionaryStore, command: DictCommand) -> Outcome {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = Cursor::new(Vec::new());
        let mut clipboard = FakeClipboard::default();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
            clipboard: &mut clipboard,
            server_url: None,
        };
        let code = match run_with(store, command, &mut io) {
            Ok(()) => 0,
            Err(error) => match error.downcast_ref::<CliError>() {
                Some(CliError::Usage(message)) => {
                    err.extend_from_slice(message.as_bytes());
                    2
                }
                Some(CliError::Failed(message)) => {
                    err.extend_from_slice(message.as_bytes());
                    1
                }
                _ => 1,
            },
        };
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn add_word(word: &str) -> DictCommand {
        DictCommand::Add {
            word: Some(word.into()),
            source: None,
            target: None,
        }
    }

    #[test]
    fn list_add_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = DictionaryStore::new(Some(dir.path().join("dictionary.json")));
        assert_eq!(
            run_dict(&store, DictCommand::List).out,
            "No dictionary entries.\n"
        );
        assert_eq!(
            run_dict(&store, add_word(" Kubernetes ")).out,
            "added: word: Kubernetes\n"
        );
        assert_eq!(
            run_dict(&store, add_word("KUBERNETES")).out,
            "already exists: word: KUBERNETES\n"
        );
        let pair = DictCommand::Add {
            word: None,
            source: Some("gee pee tee".into()),
            target: Some(" GPT ".into()),
        };
        assert_eq!(run_dict(&store, pair).out, "added: gee pee tee -> GPT\n");
        assert_eq!(
            run_dict(&store, DictCommand::List).out,
            "word: Kubernetes\ngee pee tee -> GPT\n"
        );
        let removed = run_dict(
            &store,
            DictCommand::Remove {
                word_or_from: "kubernetes".into(),
            },
        );
        assert_eq!(
            (removed.code, removed.out.as_str()),
            (0, "removed: kubernetes\n")
        );
        let missing = run_dict(
            &store,
            DictCommand::Remove {
                word_or_from: "nope".into(),
            },
        );
        assert_eq!(
            (missing.code, missing.err.as_str()),
            (1, "dictionary entry not found: nope")
        );
    }

    #[test]
    fn add_rejects_mixed_or_partial_arguments_and_blank_words() {
        let dir = tempfile::tempdir().unwrap();
        let store = DictionaryStore::new(Some(dir.path().join("dictionary.json")));
        let mixed = DictCommand::Add {
            word: Some("w".into()),
            source: Some("a".into()),
            target: Some("b".into()),
        };
        assert_eq!(run_dict(&store, mixed).code, 2);
        let partial = DictCommand::Add {
            word: None,
            source: Some("a".into()),
            target: None,
        };
        assert_eq!(run_dict(&store, partial).code, 2);
        let none = DictCommand::Add {
            word: None,
            source: None,
            target: None,
        };
        assert_eq!(run_dict(&store, none).code, 2);
        let blank = run_dict(&store, add_word("   "));
        assert_eq!(blank.code, 1);
        assert!(!blank.err.is_empty());
    }

    #[test]
    fn import_reports_counts_and_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = DictionaryStore::new(Some(dir.path().join("dictionary.json")));
        let csv = dir.path().join("words.csv");
        std::fs::write(&csv, "word\nLocalFlow\nlocalflow\n,\n").unwrap();
        let ok = run_dict(&store, DictCommand::Import { file_path: csv });
        assert_eq!(ok.out, "added 1; skipped_duplicate 1; skipped_invalid 1\n");
        let missing = run_dict(
            &store,
            DictCommand::Import {
                file_path: dir.path().join("missing.csv"),
            },
        );
        assert_eq!(missing.code, 2);
        assert!(missing.err.contains("does not exist"));
        let folder = run_dict(
            &store,
            DictCommand::Import {
                file_path: dir.path().to_path_buf(),
            },
        );
        assert_eq!(folder.code, 2);
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "x").unwrap();
        let wrong = run_dict(&store, DictCommand::Import { file_path: txt });
        assert_eq!(
            (wrong.code, wrong.err.as_str()),
            (1, "import file must be CSV or JSON")
        );
    }
}
