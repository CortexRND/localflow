//! `meeting start|stop|dismiss`: the Python `lf meeting` commands, which POST
//! to the running server (`cli._post`) and print through Rich.

use std::time::Duration;

use anyhow::Result;
use lf_core::{apitoken, config};
use serde_json::{json, Value};

use crate::args::MeetingCommand;
use crate::cmd_status::{base_url, TOKEN_HEADER};
use crate::pyfmt::value_str;
use crate::richfmt::Console;
use crate::{fail, CliError, Io};

const START_SERVER_HINT: &str =
    "open [bold]LocalFlow.app[/bold] (it starts the server) or run: [bold]lf serve[/bold]";

pub(crate) fn run(command: MeetingCommand, io: &mut Io<'_>) -> Result<()> {
    let console = Console::detect();
    match command {
        MeetingCommand::Start { title, category } => {
            // click's ParameterSource: only a --title given on the command line
            // (even "Meeting") disables the automatic title.
            let auto_title = title.is_none();
            let body = json!({
                "title": title.as_deref().unwrap_or("Meeting"),
                "category": category,
                "auto_title": auto_title,
            });
            let out = post(io, &console, "/meeting/start", Some(&body))?;
            let category = field(&out, "category")?;
            write!(
                io.out,
                "{}",
                console.markup(&format!(
                    "[green]●[/green] transcribing — category [bold]{category}[/bold]. \
                     Stop with: [bold]lf meeting stop[/bold]"
                ))
            )?;
        }
        MeetingCommand::Stop => {
            // `console.status(...)` draws nothing when stdout is not a terminal.
            let out = post(io, &console, "/meeting/stop", None)?;
            let (segments, notes, log) = (
                field(&out, "segments")?,
                field(&out, "notes_path")?,
                field(&out, "log_path")?,
            );
            for line in [
                format!("[green]saved[/green] ({segments} segments)"),
                format!("  notes: {notes}"),
                format!("  log:   {log}"),
            ] {
                write!(io.out, "{}", console.markup(&line))?;
            }
        }
        MeetingCommand::Dismiss => {
            post(io, &console, "/meeting/dismiss", None)?;
            write!(io.out, "{}", console.markup("dismissed"))?;
        }
    }
    Ok(())
}

/// `out[key]` formatted with `str()`.
fn field(out: &Value, key: &str) -> Result<String> {
    match out.get(key) {
        Some(value) => Ok(value_str(value)),
        None => fail(format!("server response has no {key:?}")),
    }
}

/// `cli._post`: POST `body` as JSON (no body for `None`) with the API token;
/// exit 1 with Rich's message when the server is down or answers an error.
fn post(io: &mut Io<'_>, console: &Console, path: &str, body: Option<&Value>) -> Result<Value> {
    let loaded = config::load_config()?;
    let base = match &io.server_url {
        Some(url) => url.clone(),
        None => base_url(
            loaded.fields.get("server_host").unwrap_or(&Value::Null),
            loaded.fields.get("server_port").unwrap_or(&Value::Null),
        ),
    };
    // Read-only, like `status`: a missing token is never created here.
    let token = apitoken::read()?.filter(|token| !token.is_empty());
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(600))
        .build()?;
    let mut request = client.post(format!("{base}{path}"));
    if let Some(token) = &token {
        request = request.header(TOKEN_HEADER, token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(error) if error.is_connect() => {
            write!(
                io.out,
                "{}",
                console.markup(&format!(
                    "[red]server not running[/red] — {START_SERVER_HINT}"
                ))
            )?;
            return Err(CliError::Exit(1).into());
        }
        Err(error) => return Err(error.into()),
    };
    let status = response.status();
    let text = response.text()?;
    if status.as_u16() >= 400 {
        // `resp.json().get("detail", resp.reason)`; anything but a JSON
        // object falls back to the reason phrase.
        let reason = status.canonical_reason().unwrap_or("");
        let detail = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map
                .get("detail")
                .map(value_str)
                .unwrap_or_else(|| reason.into()),
            _ => reason.into(),
        };
        write!(
            io.out,
            "{}",
            console.markup(&format!("[red]error:[/red] {detail}"))
        )?;
        return Err(CliError::Exit(1).into());
    }
    Ok(serde_json::from_str(&text)?)
}
