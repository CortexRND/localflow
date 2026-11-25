//! Python's `server._request_is_local` and the pre-body gate of the
//! voiceprint routes (`/api/speakers*`, slice B9).
//!
//! A request is local only when the TCP peer is loopback, the `Host` header is
//! `localhost` or a loopback literal, and no reverse-proxy header is present (a
//! local proxy such as `tailscale serve` makes every peer look like loopback).
//! Without a known peer (in-process callers) a request is never local.

use axum::http::{HeaderMap, Method};
use lf_core::config::Config;
use std::net::{IpAddr, SocketAddr};

const PROXY_HEADERS: [&str; 5] = [
    "forwarded",
    "via",
    "x-real-ip",
    "true-client-ip",
    "cf-connecting-ip",
];
const PROXY_PREFIXES: [&str; 3] = ["x-forwarded-", "x-original-", "tailscale-"];

/// Request bodies of `/api/speakers*` routes other than an audio upload.
pub const MAX_SPEAKER_JSON_BYTES: usize = 64 * 1024;
/// A multipart enrollment upload: the audio cap plus room for the form.
pub const MAX_ENROLL_BODY_BYTES: usize = crate::MAX_UPLOAD_BYTES + 64 * 1024;

fn ip_is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(v6.is_loopback(), |v4| v4.is_loopback()),
    }
}

/// `urlparse(f"//{host}").hostname` is `localhost` or a loopback IP literal.
fn host_header_is_loopback(host: &str) -> bool {
    let authority = host.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let name = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((name, _)) => name,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    let name = name.to_lowercase();
    name == "localhost" || name.parse::<IpAddr>().is_ok_and(ip_is_loopback)
}

pub(crate) fn is_local(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    let Some(peer) = peer else {
        return false;
    };
    if !ip_is_loopback(peer.ip()) {
        return false;
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_header_is_loopback(host) {
        return false;
    }
    !headers.keys().any(|name| {
        let name = name.as_str();
        PROXY_HEADERS.contains(&name) || PROXY_PREFIXES.iter().any(|p| name.starts_with(p))
    })
}

pub(crate) fn is_speakers_path(path: &str) -> bool {
    path == "/api/speakers" || path.starts_with("/api/speakers/")
}

/// `split(";")[0].strip().lower()` of the request's `Content-Type`.
pub(crate) fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// How a `/api/speakers*` request may proceed before its body is read.
pub(crate) enum Gate {
    Refuse(u16, &'static str),
    /// Read at most this many body bytes (`None`: drop the body unread).
    Read(Option<usize>),
}

/// Python's order: the recognition gate, then locality, then the body.
pub(crate) fn speakers_gate(
    config: &Config,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    local: bool,
) -> Gate {
    let on = |key| config.field_bool(key) == Some(true);
    if !(on("speaker_diarization") && on("speaker_recognition")) {
        return Gate::Refuse(403, "speaker recognition is off");
    }
    if *method == Method::GET && path == "/api/speakers/export" {
        return if local {
            Gate::Read(Some(MAX_SPEAKER_JSON_BYTES))
        } else {
            Gate::Refuse(403, "export is only available on this computer")
        };
    }
    match *method {
        Method::POST => match content_type(headers).as_str() {
            "application/json" => Gate::Read(Some(MAX_SPEAKER_JSON_BYTES)),
            "multipart/form-data" if !local => Gate::Refuse(
                403,
                "enrolling from audio is only available on this computer",
            ),
            "multipart/form-data" => Gate::Read(Some(MAX_ENROLL_BODY_BYTES)),
            _ => Gate::Read(None), // 415 from the backend, body unread
        },
        Method::PATCH => Gate::Read(Some(MAX_SPEAKER_JSON_BYTES)),
        _ => Gate::Read(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn locality_needs_loopback_peer_host_and_no_proxy_headers() {
        let v4: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let v6: SocketAddr = "[::1]:5000".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:5000".parse().unwrap();
        let lan: SocketAddr = "192.168.1.9:5000".parse().unwrap();
        for host in [
            "127.0.0.1:8756",
            "localhost:8756",
            "LOCALHOST",
            "[::1]:8756",
            "127.0.0.2",
        ] {
            let h = headers(&[("host", host)]);
            for peer in [v4, v6, mapped] {
                assert!(is_local(Some(peer), &h), "{host} {peer}");
            }
            assert!(!is_local(Some(lan), &h));
            assert!(!is_local(None, &h), "no peer is never local");
        }
        for host in ["remote.example", "127.1", "10.0.0.1:8756", "[::2]", ""] {
            assert!(!is_local(Some(v4), &headers(&[("host", host)])), "{host}");
        }
        for proxy in [
            "forwarded",
            "via",
            "x-real-ip",
            "true-client-ip",
            "cf-connecting-ip",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-original-url",
            "tailscale-user-login",
        ] {
            let h = headers(&[("host", "127.0.0.1"), (proxy, "1")]);
            assert!(!is_local(Some(v4), &h), "{proxy}");
        }
    }
}
