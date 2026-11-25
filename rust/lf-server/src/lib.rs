//! Optional single-process LocalFlow HTTP host. Python remains the shipped app.
//!
//! Construct a [`ServerState`] with a runtime [`backend::Backend`] to integrate
//! independently ported features; [`serve`] uses an explicit unavailable backend.
//! Static HTML is embedded unchanged from `localflow/static` at compile time.
pub mod api;
mod auth;
pub mod backend;
mod local;
pub mod server_url;
pub mod weblaunch;

use axum::{
    body::{to_bytes, Bytes},
    extract::{ConnectInfo, Request, State},
    http::{header, Method, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    Json, Router,
};
use backend::{Backend, BackendRequest, UnavailableBackend};
use include_dir::{include_dir, Dir};
use lf_core::config::{self, Config};
use lf_providers::{api as providers, store::CoreSecrets};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
static STATIC: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../localflow/static");
static RESOURCES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/resources");

pub type ConfigUpdater = dyn Fn(&Value) -> anyhow::Result<Config> + Send + Sync;

pub struct ServerState {
    config: Mutex<Config>,
    auth: Mutex<auth::Auth>,
    backend: Arc<dyn Backend>,
    config_updater: Option<Arc<ConfigUpdater>>,
    config_reader: Option<Arc<dyn Fn() -> anyhow::Result<Config> + Send + Sync>>,
    speaker_config_reader: Option<Arc<dyn Fn() -> anyhow::Result<Config> + Send + Sync>>,
    reload_before_put: bool,
}

impl ServerState {
    pub fn while_idle(&self, action: &mut dyn FnMut()) -> bool {
        self.backend.while_idle(action)
    }

    pub fn shutdown(&self) -> anyhow::Result<()> {
        let config = self
            .config_reader
            .as_ref()
            .and_then(|reader| reader().ok())
            .unwrap_or_else(|| {
                self.config
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            });
        self.backend.shutdown(&config)
    }

    /// In-memory auth scope: replay rotates only this instance's token. For the
    /// standard on-disk token and persistent replay recovery, use `load_with_backend`.
    pub fn new(config: Config, token: String, backend: Arc<dyn Backend>) -> Self {
        Self {
            config: Mutex::new(config),
            auth: Mutex::new(auth::Auth::new(token)),
            backend,
            config_updater: None,
            config_reader: None,
            speaker_config_reader: None,
            reload_before_put: false,
        }
    }

    pub fn load() -> anyhow::Result<Self> {
        Self::load_with_backend(Arc::new(UnavailableBackend))
    }

    /// Reads the existing token (no startup rotation) and attaches replay
    /// recovery to that exact file, independent of future environment changes.
    /// `PUT /api/config` applies to the saved file, keeping edits made since startup.
    pub fn load_with_backend(backend: Arc<dyn Backend>) -> anyhow::Result<Self> {
        Ok(Self {
            config: Mutex::new(config::load_config()?),
            auth: Mutex::new(auth::Auth::load()?),
            backend,
            config_updater: None,
            config_reader: None,
            speaker_config_reader: None,
            reload_before_put: true,
        })
    }

    pub fn with_config_updater(mut self, updater: Arc<ConfigUpdater>) -> Self {
        self.config_updater = Some(updater);
        self
    }

    pub fn with_config_reader(
        mut self,
        reader: Arc<dyn Fn() -> anyhow::Result<Config> + Send + Sync>,
    ) -> Self {
        self.config_reader = Some(reader);
        self
    }

    /// Re-read the diarization settings only for the speaker-data routes
    /// (`/api/speakers*`, a meeting's speakers, `/api/diarization/status`), so
    /// the voiceprint gate follows edits to the saved file while every other
    /// setting and route keeps the server's config. A general
    /// `with_config_reader` already covers them.
    pub fn with_speaker_config_reader(
        mut self,
        reader: Arc<dyn Fn() -> anyhow::Result<Config> + Send + Sync>,
    ) -> Self {
        self.speaker_config_reader = Some(reader);
        self
    }

    /// The config a speaker-data request is judged by: freshly read with a
    /// general reader; with only a speaker reader, the server's config with
    /// just the diarization fields taken from the fresh read (so the vault and
    /// every other setting stay those meeting saves use); else the server's.
    fn speaker_config(&self) -> anyhow::Result<Config> {
        if let Some(reader) = self.config_reader.as_ref() {
            return reader();
        }
        let Some(reader) = self.speaker_config_reader.as_ref() else {
            return Ok(self.config.lock().unwrap().clone());
        };
        let live = reader()?;
        let mut config = self.config.lock().unwrap().clone();
        for field in SPEAKER_FIELDS {
            if let Some(value) = live.field(field) {
                config.set_field(field, value.clone());
            }
        }
        Ok(config)
    }
}

/// The settings a speaker reader keeps live (the diarization fields).
const SPEAKER_FIELDS: [&str; 5] = [
    "speaker_diarization",
    "speaker_recognition",
    "diarization_backend",
    "diarization_max_speakers",
    "diarize_mic",
];

/// Routes whose config must be live for the speaker-recognition gate.
fn is_speaker_data_path(path: &str) -> bool {
    local::is_speakers_path(path)
        || path == "/api/diarization/status"
        || meeting_speakers_route(path).is_some()
}

pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .fallback(handle)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ))
        .with_state(state)
}

/// Serve until Ctrl-C. Binding is independent of the persisted config, just as
/// Python CLI overrides are; `/api/config` continues to describe saved settings.
pub async fn serve(host: &str, port: u16) -> anyhow::Result<()> {
    Server::bind(host, port).await?.run().await
}

/// A bound server. Callers can report readiness only after construction succeeds.
pub struct Server {
    listener: tokio::net::TcpListener,
    state: Arc<ServerState>,
}

impl Server {
    pub async fn bind_with_state(
        host: &str,
        port: u16,
        state: Arc<ServerState>,
    ) -> anyhow::Result<Self> {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        let host = if host == "*" || host.is_empty() {
            "0.0.0.0"
        } else {
            host
        };
        let listener = tokio::net::TcpListener::bind((host, port)).await?;
        Ok(Self { listener, state })
    }

    pub async fn run_until(
        self,
        stop: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        axum::serve(
            self.listener,
            router(self.state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop)
        .await?;
        Ok(())
    }

    pub async fn bind(host: &str, port: u16) -> anyhow::Result<Self> {
        let state = Arc::new(ServerState::load()?);
        let host = host.trim().trim_start_matches('[').trim_end_matches(']');
        let host = if host == "*" || host.is_empty() {
            "0.0.0.0"
        } else {
            host
        };
        let listener = tokio::net::TcpListener::bind((host, port)).await?;
        Ok(Self { listener, state })
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn run(self) -> anyhow::Result<()> {
        axum::serve(
            self.listener,
            router(self.state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await?;
        Ok(())
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub(crate) fn error(status: u16, detail: impl Into<String>) -> Response {
    respond(status, json!({"detail": detail.into()}))
}

fn respond(status: u16, value: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(value),
    )
        .into_response()
}

fn validation(location: Value, kind: &str, message: &str, input: Value) -> Box<Response> {
    Box::new(respond(
        422,
        json!({"detail": [{"type": kind, "loc": location, "msg": message, "input": input}]}),
    ))
}

fn json_body(body: &[u8], optional: bool) -> Result<Value, Box<Response>> {
    if body.is_empty() {
        return if optional {
            Ok(json!({}))
        } else {
            Err(validation(
                json!(["body"]),
                "missing",
                "Field required",
                Value::Null,
            ))
        };
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        validation(
            json!(["body", 0]),
            "json_invalid",
            "JSON decode error",
            json!({}),
        )
    })?;
    if value.is_null() && optional {
        return Ok(json!({}));
    }
    if !value.is_object() {
        return Err(validation(
            json!(["body"]),
            "dict_type",
            "Input should be a valid dictionary",
            value,
        ));
    }
    Ok(value)
}

fn route_methods(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "/" | "/settings" => Some(&["GET", "POST"]),
        "/api/config" => Some(&["GET", "PUT"]),
        "/api/autostart" => Some(&["GET", "POST"]),
        "/openapi.json" | "/docs" | "/redoc" | "/docs/oauth2-redirect" => Some(&["GET", "HEAD"]),
        "/healthz" | "/api/status" | "/api/providers/stt" | "/api/providers/llm"
        | "/api/models" | "/api/secrets" | "/meeting/templates" | "/meeting/status"
        | "/prompts" => Some(&["GET"]),
        "/api/launch-code" | "/api/test/llm" | "/transcribe" | "/meeting/start"
        | "/meeting/notes" | "/meeting/chat" | "/api/recall" | "/meeting/stop"
        | "/meeting/dismiss" => Some(&["POST"]),
        // Voiceprints (B9); `export` also matches the `{ref}` rename/forget routes.
        "/api/speakers" => Some(&["GET", "POST", "DELETE"]),
        "/api/speakers/export" => Some(&["GET", "PATCH", "DELETE"]),
        "/api/diarization/status" => Some(&["GET"]),
        p if p
            .strip_prefix("/api/speakers/")
            .is_some_and(|s| !s.is_empty() && !s.contains('/')) =>
        {
            Some(&["PATCH", "DELETE"])
        }
        // A saved meeting's speakers (B8).
        p if meeting_speakers_route(p).is_some() => meeting_speakers_route(p),
        p if p
            .strip_prefix("/api/secrets/")
            .is_some_and(|s| !s.is_empty() && !s.contains('/')) =>
        {
            Some(&["PUT"])
        }
        p if p.strip_prefix("/prompts/").is_some_and(|s| {
            s.split('/').count() == 2 && (s.ends_with("/approve") || s.ends_with("/reject"))
        }) =>
        {
            Some(&["POST"])
        }
        _ => None,
    }
}

/// `/api/meetings/{id}/speakers` (GET), `…/speakers/merge` (POST, and PUT as
/// a speaker id) and `…/speakers/{sid}` (PUT); one non-empty segment each.
fn meeting_speakers_route(path: &str) -> Option<&'static [&'static str]> {
    let rest = path.strip_prefix("/api/meetings/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [id, "speakers"] if !id.is_empty() => Some(&["GET"]),
        [id, "speakers", "merge"] if !id.is_empty() => Some(&["PUT", "POST"]),
        [id, "speakers", sid] if !id.is_empty() && !sid.is_empty() => Some(&["PUT"]),
        _ => None,
    }
}

async fn handle(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let path = auth::scope_path(&request);
    let method = request.method().clone();
    let Some(methods) = route_methods(&path) else {
        let trimmed = path.trim_end_matches('/');
        // A redirected DELETE would land on the bulk voiceprint erase: only
        // an explicit `DELETE /api/speakers` may forget every voice.
        if method == Method::DELETE && trimmed == "/api/speakers" {
            return error(404, "Not Found");
        }
        if trimmed != path && route_methods(trimmed).is_some() {
            // The URI is still percent-encoded while `path` is decoded. Strip
            // both representations, preserving escapes in the rest of the URI.
            let mut redirect_path = request.uri().path();
            while let Some(trimmed) = redirect_path
                .strip_suffix('/')
                .or_else(|| redirect_path.strip_suffix("%2F"))
                .or_else(|| redirect_path.strip_suffix("%2f"))
            {
                redirect_path = trimmed;
            }
            let location = format!(
                "{}{}",
                redirect_path,
                request
                    .uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default()
            );
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response();
        }
        return error(404, "Not Found");
    };
    if !methods.contains(&method.as_str()) {
        let mut response = error(405, "Method Not Allowed");
        response
            .headers_mut()
            .insert(header::ALLOW, methods.join(", ").parse().unwrap());
        return response;
    }
    let page_token = request
        .extensions()
        .get::<auth::PageToken>()
        .cloned()
        .unwrap_or_default()
        .0;
    let query = auth::query_values(request.uri().query());
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let local = local::is_local(peer, request.headers());
    // Bound uploads even without Content-Length (including chunked requests).
    // Config/notes requests have a separate generous defensive ceiling.
    let mut limit = Some(if path == "/transcribe" {
        MAX_UPLOAD_BYTES + 1024 * 1024
    } else {
        8 * 1024 * 1024
    });
    if local::is_speakers_path(&path) {
        // Voiceprint routes: the recognition gate, then locality, before any
        // body byte is read (the backend re-checks the gate itself).
        let reader = state.clone();
        let Ok(Ok(config)) = tokio::task::spawn_blocking(move || reader.speaker_config()).await
        else {
            return error(503, "could not load config");
        };
        match local::speakers_gate(&config, &method, &path, request.headers(), local) {
            local::Gate::Refuse(status, detail) => return error(status, detail),
            local::Gate::Read(bound) => limit = bound,
        }
    }
    let (parts, body) = request.into_parts();
    let body = match limit {
        Some(limit) => match to_bytes(body, limit).await {
            Ok(body) => body,
            Err(_) => return error(413, "upload too large"),
        },
        None => Bytes::new(),
    };
    match tokio::task::spawn_blocking(move || {
        dispatch(
            &state,
            &method,
            &path,
            query,
            parts.headers,
            body,
            page_token,
            local,
        )
    })
    .await
    {
        Ok(response) => response,
        Err(_) => error(500, "Internal Server Error"),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    state: &ServerState,
    method: &Method,
    path: &str,
    query: HashMap<String, String>,
    headers: axum::http::HeaderMap,
    body: Bytes,
    page_token: String,
    local: bool,
) -> Response {
    if path == "/healthz" {
        return Json(json!({"ok": true})).into_response();
    }
    if let Some(reader) = state.config_reader.as_ref() {
        match reader() {
            Ok(config) => *state.config.lock().unwrap() = config,
            Err(_) => return error(503, "could not load config"),
        }
    }
    if path == "/" || path == "/settings" {
        let file = if path == "/" {
            "index.html"
        } else {
            "settings.html"
        };
        let marker = if path == "/" {
            "__LOCALFLOW_LAUNCH_TOKEN__"
        } else {
            "__LOCALFLOW_TOKEN__"
        };
        let page = render_page(
            STATIC.get_file(file).unwrap().contents_utf8().unwrap(),
            marker,
            &page_token,
        );
        return ([(header::CACHE_CONTROL, "no-store")], Html(page)).into_response();
    }
    if path == "/api/launch-code" {
        return match state.auth.lock().unwrap().issue() {
            Ok(code) => Json(json!({"code": code, "expires_in": auth::LAUNCH_CODE_TTL_SECONDS}))
                .into_response(),
            Err(_) => error(500, "could not issue launch code"),
        };
    }
    if path == "/api/config" && method == Method::PUT {
        let update = match json_body(&body, false) {
            Ok(v) => v,
            Err(r) => return *r,
        };
        let mut config = state.config.lock().unwrap();
        let base = if state.reload_before_put && state.config_updater.is_none() {
            match config::reload_config(&config) {
                Ok(base) => base,
                Err(_) => return error(503, "could not load config"),
            }
        } else {
            config.clone()
        };
        let updated = match config::apply_config_update(&base, &update) {
            Ok(updated) => updated,
            Err(err) => return error(400, err.to_string()),
        };
        if let Err(err) = api::validate_runtime(&updated) {
            return error(400, err);
        }
        let updated = if let Some(updater) = state.config_updater.as_ref() {
            match updater(&update) {
                Ok(updated) => updated,
                Err(_) => return error(500, "could not save config"),
            }
        } else {
            if config::save_config(&updated, None).is_err() {
                return error(500, "could not save config");
            }
            updated
        };
        *config = updated;
        let mut result = config::config_to_dict(&config);
        result["restart_required"] = json!(api::restart_required(&update));
        return Json(result).into_response();
    }
    let config = if is_speaker_data_path(path) && state.config_reader.is_none() {
        match state.speaker_config() {
            Ok(config) => config,
            Err(_) => return error(503, "could not load config"),
        }
    } else {
        state.config.lock().unwrap().clone()
    };
    match path {
        "/openapi.json" => (
            [(header::CONTENT_TYPE, "application/json")],
            RESOURCES.get_file("openapi.json").unwrap().contents(),
        )
            .into_response(),
        "/docs" | "/redoc" | "/docs/oauth2-redirect" => {
            let file = match path {
                "/docs" => "docs.html",
                "/redoc" => "redoc.html",
                _ => "oauth2-redirect.html",
            };
            Html(RESOURCES.get_file(file).unwrap().contents_utf8().unwrap()).into_response()
        }
        "/api/config" => Json(config::config_to_dict(&config)).into_response(),
        "/api/status" => Json(api::status(&config, state.backend.as_ref())).into_response(),
        "/api/providers/llm" => Json(providers::llm_providers()).into_response(),
        "/api/providers/stt" => Json(api::stt_providers(state.backend.as_ref())).into_response(),
        "/api/models" => {
            let missing: Vec<_> = ["kind", "provider"].iter().filter(|key| !query.contains_key(**key)).map(|key| json!({"type": "missing", "loc": ["query", key], "msg": "Field required", "input": null})).collect();
            if !missing.is_empty() {
                return respond(422, json!({"detail": missing}));
            }
            Json(api::models(
                &config,
                &query["kind"],
                &query["provider"],
                query.get("base_url").map(String::as_str),
            ))
            .into_response()
        }
        "/api/test/llm" => {
            let body = match json_body(&body, true) {
                Ok(v) => v,
                Err(r) => return *r,
            };
            Json(providers::test_llm(
                &config,
                body["provider"].as_str(),
                body["base_url"].as_str(),
                body["model"].as_str(),
                &CoreSecrets,
            ))
            .into_response()
        }
        "/api/secrets" => Json(api::secrets_status(&config)).into_response(),
        "/api/autostart" if method == Method::GET => {
            Json(json!({"status": state.backend.autostart_status()})).into_response()
        }
        p if p.starts_with("/api/secrets/") => {
            let body = match json_body(&body, false) {
                Ok(v) => v,
                Err(r) => return *r,
            };
            let name = p.trim_start_matches("/api/secrets/");
            if !providers::secret_names().iter().any(|n| n == name) {
                return error(404, "unknown secret");
            }
            let value = body.get("value").cloned().unwrap_or(json!(""));
            let Some(value) = value.as_str() else {
                return error(400, "value must be a string");
            };
            // Serialize with config updates: endpoint-bound keys must stay paired
            // with the exact active config until the write completes.
            let config = state.config.lock().unwrap();
            if let Err(err) = providers::update_secret(&CoreSecrets, name, value, Some(&*config)) {
                return if err.downcast_ref::<providers::InvalidKeyRef>().is_some() {
                    error(400, err.to_string())
                } else {
                    error(503, err.to_string())
                };
            }
            Json(api::secrets_status(&config)).into_response()
        }
        _ => {
            if path == "/api/autostart" {
                let body = match json_body(&body, false) {
                    Ok(v) => v,
                    Err(r) => return *r,
                };
                if !body["enabled"].is_boolean() {
                    return error(400, "enabled must be a boolean");
                }
            }
            let mut config = config;
            if path == "/api/recall" && state.reload_before_put && state.config_reader.is_none() {
                // Speaker labels follow the saved file like `lf-rs recall`, even
                // when the startup snapshot differs; an unreadable file omits them.
                let speakers = config::reload_config(&config)
                    .is_ok_and(|saved| saved.field_bool("speaker_diarization") == Some(true));
                config.set_field("speaker_diarization", json!(speakers));
            }
            let request = BackendRequest {
                method: method.clone(),
                path: path.into(),
                query,
                headers,
                body,
                config,
            };
            if method == Method::GET && path == "/api/speakers/export" {
                return match state.backend.download(request, local) {
                    Ok(download) => {
                        let mut response = download.body.into_response();
                        let headers = response.headers_mut();
                        headers
                            .insert(header::CONTENT_TYPE, download.content_type.parse().unwrap());
                        for (name, value) in download.headers {
                            if let Ok(value) = value.parse() {
                                headers.insert(name, value);
                            }
                        }
                        response
                    }
                    Err(response) => respond(response.status, response.body),
                };
            }
            let response = state.backend.handle_local(request, local);
            respond(response.status, response.body)
        }
    }
}

fn render_page(template: &str, marker: &str, token: &str) -> String {
    // Python Path.read_text uses universal newlines, including on Windows
    // checkouts where Git converts the embedded HTML to CRLF.
    template
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(marker, &weblaunch::escape_html(token))
}

#[cfg(test)]
mod openapi_tests {
    use super::*;

    /// Every documented operation is routed, and the speaker paths are documented.
    #[test]
    fn the_schema_documents_routed_operations_including_speakers() {
        let spec: Value =
            serde_json::from_slice(RESOURCES.get_file("openapi.json").unwrap().contents()).unwrap();
        let paths = spec["paths"].as_object().unwrap();
        for (path, item) in paths {
            // A template segment `{x}` becomes a concrete one.
            let concrete: String = path
                .split('/')
                .map(|s| if s.starts_with('{') { "x" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            let routed = route_methods(&concrete).unwrap_or_else(|| panic!("{path} is not routed"));
            for method in item.as_object().unwrap().keys() {
                assert!(
                    routed.contains(&method.to_uppercase().as_str()),
                    "{method} {path}"
                );
            }
        }
        let methods = |path: &str| -> Vec<&str> {
            paths[path]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect()
        };
        assert_eq!(methods("/api/diarization/status"), ["get"]);
        assert_eq!(methods("/api/meetings/{meeting_id}/speakers"), ["get"]);
        assert_eq!(
            methods("/api/meetings/{meeting_id}/speakers/{speaker_id}"),
            ["put"]
        );
        assert_eq!(
            methods("/api/meetings/{meeting_id}/speakers/merge"),
            ["post"]
        );
        assert_eq!(methods("/api/speakers"), ["get", "post", "delete"]);
        assert_eq!(methods("/api/speakers/export"), ["get"]);
        assert_eq!(methods("/api/speakers/{ref}"), ["patch", "delete"]);
        assert!(spec["paths"]["/api/speakers"]["post"]["responses"]["201"].is_object());
        // Request bodies: required, with the fields the backend reads.
        let body = |path: &str, method: &str, media: &str| -> (Vec<String>, Value) {
            let request = &paths[path][method]["requestBody"];
            assert_eq!(request["required"], true, "{method} {path}");
            let schema = request["content"][media]["schema"].clone();
            let mut fields: Vec<String> = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{method} {path} {media}"))
                .keys()
                .cloned()
                .collect();
            fields.sort();
            let mut required: Vec<String> =
                serde_json::from_value(schema["required"].clone()).unwrap();
            required.sort();
            assert_eq!(required, fields, "{method} {path} {media}");
            (fields, schema)
        };
        let (fields, json_enroll) = body("/api/speakers", "post", "application/json");
        assert_eq!(fields, ["meeting_id", "name", "speaker_id"]);
        assert_eq!(
            json_enroll["properties"]["meeting_id"]["pattern"],
            "^[0-9a-f]{32}$"
        );
        let (fields, upload) = body("/api/speakers", "post", "multipart/form-data");
        assert_eq!(fields, ["file", "name"]);
        assert_eq!(upload["properties"]["file"]["format"], "binary");
        assert_eq!(
            paths["/api/speakers"]["post"]["requestBody"]["content"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        let (fields, _) = body("/api/speakers/{ref}", "patch", "application/json");
        assert_eq!(fields, ["name"]);
        // The export is a ZIP attachment, never JSON.
        let export = &paths["/api/speakers/export"]["get"]["responses"]["200"];
        let media: Vec<&str> = export["content"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(media, ["application/zip"]);
        assert_eq!(
            export["content"]["application/zip"]["schema"]["format"],
            "binary"
        );
        assert!(export["headers"]["Content-Disposition"].is_object());
        assert!(
            !paths.contains_key("/api/diarization/download"),
            "not served by Rust"
        );
    }
}

#[cfg(test)]
mod page_tests {
    #[test]
    fn crlf_checkouts_match_python_universal_newlines() {
        assert_eq!(
            super::render_page("<html>\r\nTOKEN\r</html>\n", "TOKEN", "a&b"),
            "<html>\na&amp;b\n</html>\n"
        );
    }
}
