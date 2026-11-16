use crate::{error, ServerState};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, Method},
    middleware::Next,
    response::Response,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

pub const LAUNCH_CODE_TTL_SECONDS: u64 = 60;
const MAX_CODES: usize = 32;

/// Intentionally has no Debug implementation: these values are credentials.
pub(crate) struct Auth {
    pub token: String,
    pending: VecDeque<(String, Instant)>,
    used: VecDeque<(String, Instant)>,
}

impl Auth {
    pub fn new(token: String) -> Self {
        Self {
            token,
            pending: VecDeque::new(),
            used: VecDeque::new(),
        }
    }

    pub fn issue(&mut self) -> anyhow::Result<String> {
        self.issue_at(Instant::now())
    }

    fn issue_at(&mut self, now: Instant) -> anyhow::Result<String> {
        let code = lf_core::apitoken::generate_token()?;
        Self::prune(&mut self.pending, now);
        self.pending.push_back((
            code.clone(),
            now + Duration::from_secs(LAUNCH_CODE_TTL_SECONDS),
        ));
        Ok(code)
    }

    fn prune(codes: &mut VecDeque<(String, Instant)>, now: Instant) {
        codes.retain(|(_, expiry)| *expiry > now);
        while codes.len() >= MAX_CODES {
            codes.pop_front();
        }
    }

    fn redeem(&mut self, code: &str, now: Instant) -> Redeemed {
        if let Some(i) = self.pending.iter().position(|(c, _)| c == code) {
            let (_, expiry) = self.pending.remove(i).unwrap();
            if expiry > now {
                Self::prune(&mut self.used, now);
                self.used.push_back((
                    code.into(),
                    now + Duration::from_secs(LAUNCH_CODE_TTL_SECONDS),
                ));
                return Redeemed::Ok(self.token.clone());
            }
        }
        if let Some(i) = self.used.iter().position(|(c, _)| c == code) {
            if self.used.remove(i).unwrap().1 > now {
                return Redeemed::Reused;
            }
        }
        Redeemed::Invalid
    }

    fn rotate(&mut self) {
        self.pending.clear();
        self.used.clear();
        // Rotation is explicit theft recovery, never a startup/read side effect.
        // Fail closed even if obtaining replacement randomness fails.
        self.token = lf_core::apitoken::load_or_create().unwrap_or_default();
    }
}

enum Redeemed {
    Ok(String),
    Reused,
    Invalid,
}

/// Token proved by THIS request. Pages must never re-read the current token:
/// another request may have rotated it in the meantime.
#[derive(Clone, Default)]
pub(crate) struct PageToken(pub String);

pub(crate) fn query_values(query: Option<&str>) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

pub(crate) fn scope_path(request: &Request) -> String {
    // ASGI scope['path'] is percent-decoded; URI/Host reconstruction is unsafe.
    percent_encoding::percent_decode_str(request.uri().path())
        .decode_utf8_lossy()
        .into_owned()
}

fn loopback_host(host: &str) -> bool {
    // urllib.parse does not normalize IPv4 shorthand or percent-decode hosts.
    // A WHATWG URL parser would turn 127.1 into 127.0.0.1 here.
    let host = host.replace(['\t', '\r', '\n'], "");
    let authority = host.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(host, _)| host == "::1");
    }
    if authority.contains(['[', ']']) {
        return false;
    }
    matches!(
        authority
            .split(':')
            .next()
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "localhost" | "127.0.0.1"
    )
}

pub(crate) async fn authenticate(
    State(state): State<Arc<ServerState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = scope_path(&request);
    request.extensions_mut().insert(PageToken::default());
    if request.method() == Method::POST && matches!(path.as_str(), "/" | "/settings") {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if content_type != "application/x-www-form-urlencoded" {
            return error(401, "launch link expired or already used");
        }
        let (parts, body) = request.into_parts();
        let Ok(body) = to_bytes(body, 1024).await else {
            return error(401, "launch link expired or already used");
        };
        let code = url::form_urlencoded::parse(&body)
            .find(|(k, v)| k == "launch" && !v.is_empty())
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let token = {
            let mut auth = state.auth.lock().unwrap();
            match auth.redeem(&code, Instant::now()) {
                Redeemed::Ok(token) => token,
                Redeemed::Reused => {
                    auth.rotate();
                    return error(401, "launch link expired or already used");
                }
                Redeemed::Invalid => return error(401, "launch link expired or already used"),
            }
        };
        request = Request::from_parts(parts, body.into());
        request.extensions_mut().insert(PageToken(token));
        return next.run(request).await;
    }
    if matches!(path.as_str(), "/" | "/healthz" | "/favicon.ico") {
        return next.run(request).await;
    }
    let query = query_values(request.uri().query());
    let supplied = request
        .headers()
        .get("X-Localflow-Token")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.is_empty())
        .or_else(|| query.get("token").map(String::as_str))
        .unwrap_or_default();
    let valid = {
        let auth = state.auth.lock().unwrap();
        !supplied.is_empty()
            && !auth.token.is_empty()
            && bool::from(supplied.as_bytes().ct_eq(auth.token.as_bytes()))
    };
    if !valid {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        return if loopback_host(host) {
            error(401, "invalid API token")
        } else {
            error(403, "invalid host")
        };
    }
    let token = supplied.to_string();
    request.extensions_mut().insert(PageToken(token));
    if path == "/transcribe"
        && request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_default()
            > crate::MAX_UPLOAD_BYTES as u64
    {
        return error(413, "upload too large");
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_expire_are_single_use_and_bounded() {
        let now = Instant::now();
        let mut auth = Auth::new("original".into());
        let first = auth.issue_at(now).unwrap();
        for _ in 0..MAX_CODES {
            auth.issue_at(now).unwrap();
        }
        assert_eq!(auth.pending.len(), MAX_CODES);
        assert!(matches!(auth.redeem(&first, now), Redeemed::Invalid));
        let code = auth.issue_at(now).unwrap();
        assert!(matches!(auth.redeem(&code, now), Redeemed::Ok(t) if t == "original"));
        assert!(matches!(auth.redeem(&code, now), Redeemed::Reused));
        assert!(matches!(auth.redeem(&code, now), Redeemed::Invalid));
        let code = auth.issue_at(now).unwrap();
        assert!(matches!(
            auth.redeem(&code, now + Duration::from_secs(61)),
            Redeemed::Invalid
        ));
    }
}
