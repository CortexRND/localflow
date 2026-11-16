//! Optional single-process LocalFlow HTTP host. Python remains the shipped app.
//!
//! Construct a [`ServerState`] with a runtime [`backend::Backend`] to integrate
//! independently ported features; [`serve`] uses an explicit unavailable backend.
//! Static HTML is embedded unchanged from `localflow/static` at compile time.
pub mod api;
mod auth;
pub mod backend;
pub mod server_url;
pub mod weblaunch;

use axum::{
    body::{to_bytes, Bytes},
    extract::{Request, State},
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
    sync::{Arc, Mutex},
};

pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
static STATIC: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../localflow/static");
static RESOURCES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/resources");

pub struct ServerState {
    config: Mutex<Config>,
    auth: Mutex<auth::Auth>,
    backend: Arc<dyn Backend>,
}

impl ServerState {
    /// In-memory auth scope: replay rotates only this instance's token. For the
    /// standard on-disk token and persistent replay recovery, use `load_with_backend`.
    pub fn new(config: Config, token: String, backend: Arc<dyn Backend>) -> Self {
        Self {
            config: Mutex::new(config),
            auth: Mutex::new(auth::Auth::new(token)),
            backend,
        }
    }

    pub fn load() -> anyhow::Result<Self> {
        Self::load_with_backend(Arc::new(UnavailableBackend))
    }

    /// Reads the existing token (no startup rotation) and attaches replay
    /// recovery to that exact file, independent of future environment changes.
    pub fn load_with_backend(backend: Arc<dyn Backend>) -> anyhow::Result<Self> {
        Ok(Self {
            config: Mutex::new(config::load_config()?),
            auth: Mutex::new(auth::Auth::load()?),
            backend,
        })
    }
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
        axum::serve(self.listener, router(self.state))
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

async fn handle(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let path = auth::scope_path(&request);
    let method = request.method().clone();
    let Some(methods) = route_methods(&path) else {
        let trimmed = path.trim_end_matches('/');
        if trimmed != path && route_methods(trimmed).is_some() {
            let location = format!(
                "{}{}",
                request.uri().path().trim_end_matches('/'),
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
    let (parts, body) = request.into_parts();
    // Bound uploads even without Content-Length (including chunked requests).
    // Config/notes requests have a separate generous defensive ceiling.
    let limit = if path == "/transcribe" {
        MAX_UPLOAD_BYTES + 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    let Ok(body) = to_bytes(body, limit).await else {
        return error(413, "upload too large");
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
        )
    })
    .await
    {
        Ok(response) => response,
        Err(_) => error(500, "Internal Server Error"),
    }
}

fn dispatch(
    state: &ServerState,
    method: &Method,
    path: &str,
    query: HashMap<String, String>,
    headers: axum::http::HeaderMap,
    body: Bytes,
    page_token: String,
) -> Response {
    if path == "/healthz" {
        return Json(json!({"ok": true})).into_response();
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
        let page = STATIC
            .get_file(file)
            .unwrap()
            .contents_utf8()
            .unwrap()
            .replace(marker, &weblaunch::escape_html(&page_token));
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
        let updated = match config::apply_config_update(&config, &update) {
            Ok(updated) => updated,
            Err(err) => return error(400, err.to_string()),
        };
        if let Err(err) = api::validate_runtime(&updated) {
            return error(400, err);
        }
        if config::save_config(&updated, None).is_err() {
            return error(500, "could not save config");
        }
        *config = updated;
        let mut result = config::config_to_dict(&config);
        result["restart_required"] = json!(api::restart_required(&update));
        return Json(result).into_response();
    }
    let config = state.config.lock().unwrap().clone();
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
            let response = state.backend.handle(BackendRequest {
                method: method.clone(),
                path: path.into(),
                query,
                headers,
                body,
                config,
            });
            respond(response.status, response.body)
        }
    }
}
