//! `token`: the Python `lf token`. Reads the API token; never creates it.

use anyhow::Result;
use lf_core::apitoken;

use crate::richfmt::Console;
use crate::{outln, CliError, Io};

const NO_TOKEN: &str = "[red]no API token yet[/red] — start the server once: \
     open [bold]LocalFlow.app[/bold] or run [bold]lf serve[/bold]";

pub(crate) fn run(io: &mut Io<'_>) -> Result<()> {
    match apitoken::read()? {
        Some(token) => outln!(io.out, "{token}"),
        None => {
            write!(io.out, "{}", Console::detect().markup(NO_TOKEN))?;
            Err(CliError::Exit(1).into())
        }
    }
}
