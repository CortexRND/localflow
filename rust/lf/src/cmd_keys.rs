//! `keys list` and `keys import` (localflow/cli.py `keys` group). Values are
//! only ever shown masked.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Result;
use lf_providers::keys::{detect_imports, get_preset};
use lf_providers::{ProviderKeys, PRESETS};

use crate::args::KeysCommand;
use crate::prompt::{self, Answer};
use crate::pyfmt::str_repr;
use crate::{fail, outln, usage, CliError, Io};

pub(crate) fn run(command: KeysCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        KeysCommand::List => list(io),
        KeysCommand::Import { file, yes } => import(file.as_deref(), yes, io),
    }
}

fn list(io: &mut Io<'_>) -> Result<()> {
    let keys = ProviderKeys::system();
    for preset in PRESETS.iter() {
        let status = keys.key_status(preset.id)?;
        let value = if !status.masked_value.is_empty() {
            status.masked_value.as_str()
        } else if preset.needs_key {
            "No key"
        } else {
            "No key needed"
        };
        outln!(io.out, "{}: {value}", preset.label)?;
    }
    Ok(())
}

fn import(file: Option<&Path>, yes: bool, io: &mut Io<'_>) -> Result<()> {
    if let Some(file) = file {
        check_file_option(file)?;
    }
    // The process environment; entries that are not Unicode can't be keys.
    // Windows names are case-insensitive: upper-cased, as in Python's `os.environ`.
    let environ: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .map(|(name, value)| match cfg!(windows) {
            true => (name.to_uppercase(), value),
            false => (name, value),
        })
        .collect();
    let Ok(candidates) = detect_imports(file, &environ) else {
        return fail("Unable to read the selected key file");
    };
    if candidates.is_empty() {
        return outln!(io.out, "No keys found");
    }
    let mut confirmed = Vec::new();
    for candidate in candidates {
        let preset = get_preset(&candidate.provider_id)?;
        outln!(
            io.out,
            "{}: {} ({})",
            preset.label,
            candidate.masked_value(),
            candidate.source
        )?;
        let save = yes
            || match prompt::confirm("Save this key?", io)? {
                Answer::Yes => true,
                Answer::No => false,
                Answer::Aborted => {
                    outln!(io.err, "Aborted!")?;
                    return Err(CliError::Exit(1).into());
                }
            };
        if save {
            confirmed.push(candidate);
        }
    }
    if ProviderKeys::system().save_imports(&confirmed).is_err() {
        return fail("Unable to save keys; check Keychain access");
    }
    outln!(io.out, "Saved {} key(s)", confirmed.len())
}

/// `click.Path(exists=True, dir_okay=False)` on `--file`: usage errors.
fn check_file_option(file: &Path) -> Result<()> {
    let shown = str_repr(&file.to_string_lossy());
    let problem = match std::fs::metadata(file) {
        Err(_) => "does not exist",
        Ok(meta) if meta.is_dir() => "is a directory",
        Ok(_) => match std::fs::File::open(file) {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => "is not readable",
            _ => return Ok(()),
        },
    };
    usage(format!(
        "Invalid value for '--file': File {shown} {problem}."
    ))
}
