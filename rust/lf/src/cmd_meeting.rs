//! `meeting start|stop|dismiss`: the Python `lf meeting` commands, which POST
//! to the running server (`cli._post`) and print through Rich.

use std::time::Duration;

use anyhow::Result;
use lf_core::{apitoken, config};
use reqwest::Method;
use serde_json::{json, Value};

use crate::args::MeetingCommand;
use crate::cmd_status::{base_url, TOKEN_HEADER};
use crate::pyfmt::value_str;
use crate::richfmt::Console;
use crate::{fail, CliError, Io};

pub(crate) const START_SERVER_HINT: &str =
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
        command => return speakers_command(command, io, &console),
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
pub(crate) fn post(
    io: &mut Io<'_>,
    console: &Console,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    send(io, console, Method::POST, path, body, Errors::Console)
}

/// How an error answer is shown: `Console` is `_post`/`_put`/`_patch`/`_delete`
/// (Rich `error:` on stdout); `Stderr` is a caught `_get` `HTTPError`
/// (`click.echo(f"error: …", err=True)`).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Errors {
    Console,
    Stderr,
}

/// The configured server URL (or the test override).
pub(crate) fn server_base(io: &Io<'_>) -> Result<String> {
    if let Some(url) = &io.server_url {
        return Ok(url.clone());
    }
    let loaded = config::load_config()?;
    Ok(base_url(
        loaded.fields.get("server_host").unwrap_or(&Value::Null),
        loaded.fields.get("server_port").unwrap_or(&Value::Null),
    ))
}

/// `X-Localflow-Token` when a token exists; read-only, like `status`.
pub(crate) fn token() -> Result<Option<String>> {
    Ok(apitoken::read()?.filter(|token| !token.is_empty()))
}

/// The CLI's HTTP helpers (`_get`, `_post`, `_put`, `_patch`, `_delete`):
/// `method` with `body` as JSON and the API token.
pub(crate) fn send(
    io: &mut Io<'_>,
    console: &Console,
    method: Method,
    path: &str,
    body: Option<&Value>,
    errors: Errors,
) -> Result<Value> {
    request(io, console, method, path, body, errors, true)
}

/// A speaker-data change (rename, merge, enroll, forget): like [`send`] with
/// console errors, but a redirect is refused, never followed — a followed
/// 307 would replay the mutation on whatever URL the answer names.
pub(crate) fn mutate(
    io: &mut Io<'_>,
    console: &Console,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value> {
    request(io, console, method, path, body, Errors::Console, false)
}

/// Refuse a 3xx answer with the CLI's redirect message (exit 1).
pub(crate) fn refuse_redirect(
    io: &mut Io<'_>,
    console: &Console,
    status: reqwest::StatusCode,
) -> Result<()> {
    if status.is_redirection() {
        write!(
            io.out,
            "{}",
            console.markup("[red]error:[/red] the server answered with a redirect; refusing it")
        )?;
        return Err(CliError::Exit(1).into());
    }
    Ok(())
}

/// One path segment of a speaker or meeting reference, percent-encoded; an
/// empty, `.` or `..` reference is a usage error before any request (it would
/// otherwise address another route, e.g. the bulk `DELETE /api/speakers`).
pub(crate) fn segment(value: &str, what: &str) -> Result<String> {
    if matches!(value, "" | "." | "..") {
        return crate::usage(format!(
            "invalid {what}: {}",
            lf_core::symbols::pycompat::repr_str(value)
        ));
    }
    Ok(quote(value))
}

fn request(
    io: &mut Io<'_>,
    console: &Console,
    method: Method,
    path: &str,
    body: Option<&Value>,
    errors: Errors,
    follow_redirects: bool,
) -> Result<Value> {
    let base = server_base(io)?;
    let timeout = match method {
        Method::GET => 5,
        Method::PUT | Method::PATCH => 30,
        Method::DELETE => 60,
        _ => 600,
    };
    let mut client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout));
    if !follow_redirects {
        client = client.redirect(reqwest::redirect::Policy::none());
    }
    let client = client.build()?;
    let mut request = client.request(method, format!("{base}{path}"));
    if let Some(token) = token()? {
        request = request.header(TOKEN_HEADER, token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send();
    let response = connected(io, console, response)?;
    let status = response.status();
    if !follow_redirects {
        refuse_redirect(io, console, status)?;
    }
    let text = response.text()?;
    answer(io, console, status, &text, errors)
}

/// The response, or Rich's "server not running" and exit 1.
pub(crate) fn connected(
    io: &mut Io<'_>,
    console: &Console,
    response: reqwest::Result<reqwest::blocking::Response>,
) -> Result<reqwest::blocking::Response> {
    match response {
        Ok(response) => Ok(response),
        Err(error) if error.is_connect() => {
            write!(
                io.out,
                "{}",
                console.markup(&format!(
                    "[red]server not running[/red] — {START_SERVER_HINT}"
                ))
            )?;
            Err(CliError::Exit(1).into())
        }
        Err(error) => Err(error.into()),
    }
}

/// `resp.json().get("detail", resp.reason)`; anything but a JSON object falls
/// back to the reason phrase.
pub(crate) fn error_detail(status: reqwest::StatusCode, text: &str) -> String {
    let reason = status.canonical_reason().unwrap_or("");
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => map
            .get("detail")
            .map(value_str)
            .unwrap_or_else(|| reason.into()),
        _ => reason.into(),
    }
}

/// A success body as JSON, or the error shown in `errors` style and exit 1.
pub(crate) fn answer(
    io: &mut Io<'_>,
    console: &Console,
    status: reqwest::StatusCode,
    text: &str,
    errors: Errors,
) -> Result<Value> {
    if status.as_u16() >= 400 {
        let detail = error_detail(status, text);
        match errors {
            Errors::Console => write!(
                io.out,
                "{}",
                console.markup(&format!("[red]error:[/red] {detail}"))
            )?,
            Errors::Stderr => writeln!(io.err, "error: {detail}")?,
        }
        return Err(CliError::Exit(1).into());
    }
    Ok(serde_json::from_str(text)?)
}

/// `urllib.parse.quote(value, safe='')`.
pub(crate) fn quote(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `_meeting_id_or_last`: the given id, else the last saved meeting's.
fn meeting_id_or_last(io: &mut Io<'_>, console: &Console, id: Option<String>) -> Result<String> {
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        return Ok(id);
    }
    let status = send(
        io,
        console,
        Method::GET,
        "/meeting/status",
        None,
        Errors::Stderr,
    )?;
    match status["last_saved"]["meeting_id"]
        .as_str()
        .filter(|id| !id.is_empty())
    {
        Some(id) => Ok(id.to_string()),
        None => {
            writeln!(
                io.err,
                "error: no saved meeting with speaker labels — pass a meeting ID"
            )?;
            Err(CliError::Exit(1).into())
        }
    }
}

/// `_id_and_pair`: `[ID] A B`.
fn id_and_pair(args: Vec<String>, usage: &str) -> Result<(Option<String>, String, String)> {
    let mut args = args.into_iter();
    match (args.next(), args.next(), args.next(), args.next()) {
        (Some(a), Some(b), None, _) => Ok((None, a, b)),
        (Some(id), Some(a), Some(b), None) => Ok((Some(id), a, b)),
        _ => crate::usage(format!("expected {usage}")),
    }
}

/// `_echo_speakers`.
fn echo_speakers(io: &mut Io<'_>, out: &Value) -> Result<()> {
    writeln!(io.out, "meeting {}", value_str(&out["meeting_id"]))?;
    for s in out["speakers"].as_array().into_iter().flatten() {
        writeln!(
            io.out,
            "{}\t{}\t{:.1}s\t{}",
            value_str(&s["id"]),
            value_str(&s["label"]),
            s["seconds"].as_f64().unwrap_or(0.0),
            value_str(&s["source"])
        )?;
    }
    Ok(())
}

fn speakers_command(command: MeetingCommand, io: &mut Io<'_>, console: &Console) -> Result<()> {
    let out = match command {
        MeetingCommand::Speakers { meeting_id } => {
            if let Some(id) = meeting_id.as_deref().filter(|id| !id.is_empty()) {
                segment(id, "meeting ID")?;
            }
            let id = meeting_id_or_last(io, console, meeting_id)?;
            let path = format!("/api/meetings/{}/speakers", segment(&id, "meeting ID")?);
            send(io, console, Method::GET, &path, None, Errors::Stderr)?
        }
        MeetingCommand::RenameSpeaker { args } => {
            let (id, speaker, name) = id_and_pair(args, "[ID] SPEAKER NAME")?;
            // Every reference is checked before any request.
            let speaker = segment(&speaker, "speaker ID")?;
            if let Some(id) = id.as_deref().filter(|id| !id.is_empty()) {
                segment(id, "meeting ID")?;
            }
            let id = meeting_id_or_last(io, console, id)?;
            let path = format!(
                "/api/meetings/{}/speakers/{speaker}",
                segment(&id, "meeting ID")?
            );
            let body = json!({"name": name});
            mutate(io, console, Method::PUT, &path, Some(&body))?
        }
        MeetingCommand::MergeSpeakers { args } => {
            let (id, from, into) = id_and_pair(args, "[ID] FROM INTO")?;
            if let Some(id) = id.as_deref().filter(|id| !id.is_empty()) {
                segment(id, "meeting ID")?;
            }
            let id = meeting_id_or_last(io, console, id)?;
            let path = format!(
                "/api/meetings/{}/speakers/merge",
                segment(&id, "meeting ID")?
            );
            let body = json!({"from": from, "into": into});
            mutate(io, console, Method::POST, &path, Some(&body))?
        }
        _ => unreachable!("not a speaker command"),
    };
    echo_speakers(io, &out)
}
