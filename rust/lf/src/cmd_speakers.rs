//! `speakers status|list|enroll|rename|forget|export`: the Python
//! `lf speakers` commands (`cli.py`). Every voiceprint command goes through
//! the local server, the single writer of the registry. Raw audio uploads
//! and the export only go to a loopback server, never through an environment
//! proxy, and never follow a redirect. Model downloads are the Python
//! `lf speakers download` (both runtimes read the same verified files).

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use lf_core::{config, symbols::pycompat as py};
use lf_meetings::speakers::{self as data, bundle};
use reqwest::Method;
use serde_json::{json, Value};

use crate::args::SpeakersCommand;
use crate::cmd_meeting::{self, answer, connected, error_detail, segment, send, Errors};
use crate::cmd_status::{client_host, TOKEN_HEADER};
use crate::pyfmt::value_str;
use crate::richfmt::Console;
use crate::{usage, CliError, Io};

/// `cli.MAX_ENROLL_UPLOAD_BYTES` (= the server's upload cap).
const MAX_ENROLL_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// `click.echo(f"error: {message}", err=True); sys.exit(1)`.
fn exit_with<T>(io: &mut Io<'_>, message: &str) -> Result<T> {
    writeln!(io.err, "error: {message}")?;
    Err(CliError::Exit(1).into())
}

pub(crate) fn run(command: SpeakersCommand, io: &mut Io<'_>) -> Result<()> {
    let console = Console::detect();
    match command {
        SpeakersCommand::Status { json } => status(json, io),
        SpeakersCommand::List { json } => {
            let out = send(
                io,
                &console,
                Method::GET,
                "/api/speakers",
                None,
                Errors::Stderr,
            )?;
            if json {
                writeln!(io.out, "{}", py::json_dumps(&out, None, true))?;
                return Ok(());
            }
            let current = match out.get("embedder") {
                Some(embedder) => embedder.as_str().map(str::to_owned),
                None => {
                    let config = config::load_config()?;
                    data::registry::embedder_id_for_backend(
                        config.field_str("diarization_backend").unwrap_or_default(),
                    )
                    .map(str::to_owned)
                }
            };
            let rows = out["speakers"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                writeln!(io.out, "no enrolled speakers")?;
                return Ok(());
            }
            for s in rows {
                let mut line = format!(
                    "{}\t{}\t{}\t{}",
                    value_str(&s["name"]),
                    value_str(&s["samples"]),
                    value_str(&s["created"]),
                    value_str(&s["model_id"])
                );
                if s["model_id"].as_str() != current.as_deref() {
                    line.push_str("\tneeds re-enrollment (different voice model)");
                }
                writeln!(io.out, "{line}")?;
            }
            Ok(())
        }
        SpeakersCommand::Enroll {
            name,
            meeting_id,
            speaker_id,
            file,
            record,
        } => {
            let sources = [meeting_id.is_some(), file.is_some(), record.is_some()];
            if sources.iter().filter(|&&s| s).count() != 1 {
                return usage("use exactly one of --from-meeting, --file or --record");
            }
            if meeting_id.is_some() != speaker_id.is_some() {
                return usage("--speaker goes with --from-meeting (and is required by it)");
            }
            if let Some(file) = &file {
                // click.Path(exists=True, dir_okay=False)
                if !file.exists() {
                    return usage(format!(
                        "Invalid value for '--file': File {} does not exist.",
                        py::repr_str(&file.to_string_lossy())
                    ));
                }
                if file.is_dir() {
                    return usage(format!(
                        "Invalid value for '--file': File {} is a directory.",
                        py::repr_str(&file.to_string_lossy())
                    ));
                }
            }
            let out = if let Some(meeting_id) = meeting_id {
                let body =
                    json!({"name": name, "meeting_id": meeting_id, "speaker_id": speaker_id});
                cmd_meeting::mutate(io, &console, Method::POST, "/api/speakers", Some(&body))?
            } else {
                // Refuse a remote server before reading or recording any audio.
                let base = local_base(io)?;
                let (audio, filename) = match (&file, record) {
                    (Some(file), _) => {
                        if std::fs::metadata(file)?.len() > MAX_ENROLL_UPLOAD_BYTES {
                            exit_with(io, "audio file is larger than 25 MB")?;
                        }
                        let name = file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "audio".into());
                        (std::fs::read(file)?, name)
                    }
                    (None, Some(seconds)) => (record_voice(io, seconds)?, "recording.wav".into()),
                    (None, None) => unreachable!("one source was checked above"),
                };
                let form = Form::new(&name, &filename, audio);
                local_send(
                    io,
                    &console,
                    &base,
                    Method::POST,
                    "/api/speakers",
                    Some(form),
                )?
                .1
            };
            writeln!(io.out, "enrolled {}", value_str(&out["name"]))?;
            Ok(())
        }
        SpeakersCommand::Rename { old, new } => {
            let path = format!("/api/speakers/{}", segment(&old, "NAME_OR_ID")?);
            let body = json!({"name": new});
            let out = cmd_meeting::mutate(io, &console, Method::PATCH, &path, Some(&body))?;
            writeln!(io.out, "renamed to {}", value_str(&out["name"]))?;
            Ok(())
        }
        SpeakersCommand::Forget { name_or_id, all } => {
            if all == name_or_id.is_some() {
                return usage("pass a NAME_OR_ID or --all");
            }
            // Only --all may address the bulk `DELETE /api/speakers`.
            let path = match &name_or_id {
                Some(reference) => format!("/api/speakers/{}", segment(reference, "NAME_OR_ID")?),
                None => "/api/speakers".into(),
            };
            let out = cmd_meeting::mutate(io, &console, Method::DELETE, &path, None)?;
            let forgotten = out.get("forgotten").cloned().unwrap_or(json!(0));
            writeln!(io.out, "forgot {} speaker(s)", value_str(&forgotten))?;
            Ok(())
        }
        SpeakersCommand::Export { directory } => export(io, &console, &directory),
    }
}

/// `speakers status`: local and offline (no server).
fn status(as_json: bool, io: &mut Io<'_>) -> Result<()> {
    let config = config::load_config()?;
    let info = data::diarization_status(&lf_stt::model_storage::hf_hub_cache(None), &config);
    if as_json {
        writeln!(io.out, "{}", py::json_dumps(&info, None, true))?;
        return Ok(());
    }
    if info["models_ready"] == true {
        writeln!(io.out, "models: ready")?;
    } else {
        writeln!(io.out, "models: not ready — {}", value_str(&info["reason"]))?;
    }
    for model in info["models"].as_array().into_iter().flatten() {
        let state = if model["ready"] == true {
            "verified".to_string()
        } else {
            value_str(&model["reason"])
        };
        writeln!(io.out, "{}: {state}", value_str(&model["id"]))?;
    }
    let installed = if info["extra_installed"] == true {
        "installed"
    } else {
        "not installed"
    };
    writeln!(io.out, "runtime (sherpa-onnx): {installed}")?;
    for name in data::FLAG_NAMES {
        writeln!(io.out, "{name}: {}", info[name] == true)?;
    }
    if info["models_ready"] != true {
        // lf-rs has no downloader; say where the files come from.
        writeln!(
            io.out,
            "download: run `lf speakers download` (LocalFlow's Python CLI); lf-rs reads the same verified files"
        )?;
    }
    Ok(())
}

/// The URL's host is `localhost` or a loopback IP literal.
fn loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `_local_base`: the server URL when the configured server is on this
/// computer (`localhost` becomes the `127.0.0.1` literal); else exit 1.
fn local_base(io: &mut Io<'_>) -> Result<String> {
    const REMOTE: &str = "voice audio and exports only go to a LocalFlow server on this \
                          computer (server_host is not a loopback address)";
    if let Some(url) = io.server_url.clone() {
        let local = reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(loopback_host))
            .unwrap_or(false);
        if !local {
            exit_with(io, REMOTE)?;
        }
        return Ok(url);
    }
    let config = config::load_config()?;
    let mut host = client_host(config.fields.get("server_host").unwrap_or(&Value::Null));
    if host.eq_ignore_ascii_case("localhost") {
        host = "127.0.0.1".into();
    }
    if !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        exit_with(io, REMOTE)?;
    }
    Ok(crate::cmd_status::base_url(
        &json!(host),
        config.fields.get("server_port").unwrap_or(&Value::Null),
    ))
}

/// A `multipart/form-data` body with a `name` field and one `file` upload,
/// as `requests` sends `data={"name": …}, files={"file": (…)}`.
struct Form {
    boundary: String,
    body: Vec<u8>,
}
impl Form {
    fn new(name: &str, filename: &str, data: Vec<u8>) -> Self {
        let boundary = lf_core::compact_uuid_v4();
        // HTML5 form escaping of the quoted file name (as urllib3 does).
        let filename = filename
            .replace('"', "%22")
            .replace('\r', "%0D")
            .replace('\n', "%0A");
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Self { boundary, body }
    }
}

/// `_local_send`: no environment proxy, no redirects (a 3xx is refused).
fn local_send(
    io: &mut Io<'_>,
    console: &Console,
    base: &str,
    method: Method,
    path: &str,
    form: Option<Form>,
) -> Result<(Vec<u8>, Value)> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(600))
        .build()?;
    let mut request = client.request(method, format!("{base}{path}"));
    if let Some(token) = cmd_meeting::token()? {
        request = request.header(TOKEN_HEADER, token);
    }
    if let Some(form) = form {
        request = request
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", form.boundary),
            )
            .body(form.body);
    }
    let response = connected(io, console, request.send())?;
    let status = response.status();
    cmd_meeting::refuse_redirect(io, console, status)?;
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json"));
    // Bounded: a valid export is at most 64 MiB.
    let mut body = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(response, bundle::MAX_EXPORT_BYTES as u64 + 1),
        &mut body,
    )?;
    if !status.is_success() {
        let detail = error_detail(status, &String::from_utf8_lossy(&body));
        write!(
            io.out,
            "{}",
            console.markup(&format!("[red]error:[/red] {detail}"))
        )?;
        return Err(CliError::Exit(1).into());
    }
    let value = if is_json {
        answer(
            io,
            console,
            status,
            &String::from_utf8_lossy(&body),
            Errors::Console,
        )?
    } else {
        Value::Null
    };
    Ok((body, value))
}

/// `_inside_vault`: the target (as given or resolved) is the vault or inside
/// it; anything undeterminable counts as inside.
fn inside_vault(target: &Path) -> bool {
    let Ok(config) = config::load_config() else {
        return true;
    };
    let vault = config.field_str("vault_path").unwrap_or_default();
    if vault.is_empty() {
        return false;
    }
    let Ok(vault) = lf_core::home::expand_user(Path::new(vault)) else {
        return true;
    };
    let absolute = |p: &Path| -> Option<PathBuf> {
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            std::env::current_dir().ok().map(|d| d.join(p))
        }
    };
    let resolved = |p: &Path| -> Option<PathBuf> {
        // Python's non-strict resolve: the longest existing ancestor, resolved.
        let p = absolute(p)?;
        let mut existing = p.as_path();
        let mut rest = Vec::new();
        while !existing.exists() {
            rest.push(existing.file_name()?.to_owned());
            existing = existing.parent()?;
        }
        let mut out = std::fs::canonicalize(existing).ok()?;
        out.extend(rest.iter().rev());
        Some(out)
    };
    let (Some(ta), Some(tr), Some(va), Some(vr)) = (
        absolute(target),
        resolved(target),
        absolute(&vault),
        resolved(&vault),
    ) else {
        return true;
    };
    [&ta, &tr]
        .iter()
        .any(|t| [&va, &vr].iter().any(|v| t.starts_with(v)))
}

fn export(io: &mut Io<'_>, console: &Console, directory: &Path) -> Result<()> {
    let target = lf_core::home::expand_user(directory).unwrap_or_else(|_| directory.to_path_buf());
    if inside_vault(&target) {
        return exit_with(io, "don't export voiceprints into the Obsidian vault");
    }
    if target.exists()
        && (!target.is_dir()
            || std::fs::read_dir(&target)
                .map(|mut d| d.next().is_some())
                .unwrap_or(true))
    {
        return exit_with(io, "the export folder must be new or empty");
    }
    let base = local_base(io)?;
    let (zip, _) = local_send(
        io,
        console,
        &base,
        Method::GET,
        "/api/speakers/export",
        None,
    )?;
    match bundle::write_bundle(&zip, &target) {
        Ok((dir, count)) => {
            writeln!(io.out, "exported {count} speaker(s) to {}", dir.display())?;
            writeln!(io.out, "These files are voiceprints. Keep them private.")?;
            Ok(())
        }
        Err(data::RegistryError::Io(_)) => exit_with(io, "could not write"),
        Err(error) => exit_with(io, &error.to_string()),
    }
}

/// `_record_voice`: `seconds` of the configured microphone as a 16-bit PCM
/// mono WAV (16 kHz), held in memory only.
fn record_voice(io: &mut Io<'_>, seconds: u32) -> Result<Vec<u8>> {
    use lf_meetings::{
        system_audio::{CpalSource, SAMPLE_RATE},
        AudioSource,
    };
    use std::sync::{Arc, Mutex};
    let config = config::load_config()?;
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink = samples.clone();
    let mut mic = match CpalSource::microphone_named(config.field_str("input_device")) {
        Ok(mic) => mic,
        Err(error) => return exit_with(io, &format!("could not open the microphone: {error}")),
    };
    if let Err(error) = mic.start(Arc::new(move |audio: Vec<f32>| {
        sink.lock().unwrap_or_else(|e| e.into_inner()).extend(audio)
    })) {
        return exit_with(io, &format!("could not open the microphone: {error}"));
    }
    if io.terminal {
        let _ = writeln!(io.err, "recording {seconds}s — speak normally…");
    }
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    let _ = mic.stop();
    drop(mic);
    let audio = std::mem::take(&mut *samples.lock().unwrap_or_else(|e| e.into_inner()));
    if audio.is_empty() {
        exit_with(io, "nothing was recorded")?;
    }
    Ok(wav_bytes(&audio, SAMPLE_RATE))
}

/// `_wav_bytes`: 16-bit PCM mono WAV.
fn wav_bytes(audio: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (audio.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + audio.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in audio {
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_16_bit_mono_pcm() {
        let wav = wav_bytes(&[0.0, 1.0, -1.0, 2.0], 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        let samples: Vec<i16> = wav[44..]
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, [0, 32767, -32767, 32767]);
    }

    #[test]
    fn only_loopback_hosts_are_local() {
        for host in ["127.0.0.1", "localhost", "[::1]", "127.9.9.9"] {
            assert!(loopback_host(host), "{host}");
        }
        for host in ["192.168.1.2", "example.com", "0.0.0.0", "127.1"] {
            assert!(!loopback_host(host), "{host}");
        }
    }
}
