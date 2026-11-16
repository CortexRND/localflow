//! `keys list` (localflow/cli.py `keys` group). Values are only ever shown
//! masked; nothing is written.

use anyhow::Result;
use lf_providers::{ProviderKeys, PRESETS};

use crate::args::KeysCommand;
use crate::{outln, Io};

pub(crate) fn run(command: KeysCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        KeysCommand::List => list(io),
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
