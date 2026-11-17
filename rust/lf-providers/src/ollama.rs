//! Port of `localflow/providers/llm_ollama.py`.

use crate::base::{Health, LlmProvider, ModelInfo, ProviderError};
use crate::http;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub struct OllamaLlm {
    pub base_url: String,
    pub model: String,
    pub timeout: i64,
    pub num_ctx: i64,
}

pub const DEFAULT_TIMEOUT: i64 = 120;
pub const DEFAULT_NUM_CTX: i64 = 8192;
pub const HEALTH_TIMEOUT: i64 = 3;

impl OllamaLlm {
    /// Stream a real pull to completion; EOF without success is an error.
    pub fn pull(
        &self,
        progress: &(dyn Fn(u64, u64, &str) + Send + Sync),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), ProviderError> {
        crate::transfer::run(cancel, async {
            if self.model.trim().is_empty() {
                return Err("Choose an Ollama model.".into());
            }
            let client = crate::transfer::client(&self.base_url)?;
            let mut response = client
                .post(format!("{}/api/pull", self.base_url))
                .json(&json!({"model":self.model,"stream":true}))
                .send()
                .await
                .map_err(|e| {
                    let detail = crate::transfer::error(&e);
                    if e.is_connect() {
                        format!("Ollama not running: {detail}")
                    } else {
                        detail
                    }
                })?;
            let status = response.status();
            if !status.is_success() {
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|e| crate::transfer::error(&e))?
                {
                    body.extend_from_slice(&chunk[..chunk.len().min(8192 - body.len())]);
                    if body.len() == 8192 {
                        break;
                    }
                }
                return Err(format!(
                    "Ollama {status}: {}",
                    String::from_utf8_lossy(&body)
                ));
            }
            let mut pending = Vec::new();
            let mut success = false;
            let mut layers = std::collections::BTreeMap::new();
            let mut record = |line: &[u8]| -> Result<(), String> {
                if line.iter().all(u8::is_ascii_whitespace) {
                    return Ok(());
                }
                let data: serde_json::Value = serde_json::from_slice(line)
                    .map_err(|e| format!("Invalid Ollama progress: {e}"))?;
                if let Some(error) = data.get("error").and_then(|s| s.as_str()) {
                    return Err(error.into());
                }
                let status = data
                    .get("status")
                    .and_then(|s| s.as_str())
                    .ok_or("Missing Ollama progress status.")?;
                success |= status == "success";
                if let Some(digest) = data.get("digest").and_then(|s| s.as_str()) {
                    layers.insert(
                        digest.to_owned(),
                        (
                            data.get("completed").and_then(|n| n.as_u64()).unwrap_or(0),
                            data.get("total").and_then(|n| n.as_u64()).unwrap_or(0),
                        ),
                    );
                }
                let (done, total) =
                    layers
                        .values()
                        .fold((0_u64, 0_u64), |(d, t), (next_d, next_t)| {
                            (d.saturating_add(*next_d), t.saturating_add(*next_t))
                        });
                progress(done, total, status);
                Ok(())
            };
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| crate::transfer::error(&e))?
            {
                for byte in chunk {
                    pending.push(byte);
                    if pending.len() > 65536 {
                        return Err("Ollama progress record too large.".into());
                    }
                    if byte == b'\n' {
                        record(&pending)?;
                        pending.clear();
                    }
                }
            }
            if !pending.is_empty() {
                record(&pending)?;
            }
            if !success {
                return Err("Ollama pull ended without success.".into());
            }
            Ok(())
        })
        .map_err(ProviderError)
    }

    pub fn new(base_url: &str, model: &str, timeout: i64, num_ctx: i64) -> Self {
        OllamaLlm {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            timeout,
            num_ctx,
        }
    }

    /// The `/api/generate` request body.
    pub fn generate_body(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> serde_json::Value {
        let prompt = if system.is_empty() {
            user.to_string()
        } else {
            format!("{system}\n\n{user}")
        };
        json!({
            "model": self.model,
            "prompt": prompt,
            "options": {
                "num_ctx": self.num_ctx,
                "num_predict": max_tokens.unwrap_or(-1),
            },
            "stream": false,
        })
    }

    fn try_complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, String> {
        let client = http::client(self.timeout)?;
        let body = self.generate_body(system, user, max_tokens);
        let data = http::send_json(
            client
                .post(format!("{}/api/generate", self.base_url))
                .json(&body),
        )?;
        let text = match http::dict_get(&data, "response")? {
            None => "",
            Some(value) => http::as_str(value, "strip")?,
        };
        Ok(clean_text(text).to_string())
    }

    fn try_list(&self) -> Result<Vec<ModelInfo>, String> {
        let client = http::client(self.timeout)?;
        let data = http::send_json(client.get(format!("{}/api/tags", self.base_url)))?;
        let empty = serde_json::Value::Array(Vec::new());
        let models = http::dict_get(&data, "models")?.unwrap_or(&empty);
        http::iter_items(models)?
            .into_iter()
            .map(|model| {
                let name = http::dict_index(model, "name")?;
                let name = name.as_str().ok_or_else(|| {
                    format!("model name is a {}, not a string", http::py_type(name))
                })?;
                Ok(ModelInfo {
                    id: name.into(),
                    label: name.into(),
                    installed: Some(true),
                })
            })
            .collect()
    }
}

/// `text.strip().strip('"').strip("'").strip()`
fn clean_text(text: &str) -> &str {
    let text = crate::pyfmt::py_strip(text);
    let text = text.trim_matches('"');
    let text = text.trim_matches('\'');
    crate::pyfmt::py_strip(text)
}

impl LlmProvider for OllamaLlm {
    fn id(&self) -> &'static str {
        "ollama"
    }

    fn offline(&self) -> bool {
        true
    }

    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError> {
        match self.try_complete(system, user, max_tokens) {
            Ok(text) if text.is_empty() => {
                Err(ProviderError("Ollama returned an empty response".into()))
            }
            Ok(text) => Ok(text),
            Err(exc) => Err(ProviderError(format!("Ollama completion failed: {exc}"))),
        }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.try_list()
            .map_err(|exc| ProviderError(format!("Ollama model listing failed: {exc}")))
    }

    fn healthcheck(&self) -> Health {
        let result = http::client(HEALTH_TIMEOUT)
            .and_then(|client| http::send_ok(client.get(format!("{}/api/tags", self.base_url))));
        match result {
            Ok(()) => Health::ok(),
            Err(detail) => Health::failed(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockResponse, MockServer};

    fn llm(server: &MockServer) -> OllamaLlm {
        OllamaLlm::new(&format!("{}/", server.url()), "llama3.2:3b", 5, 4096)
    }

    #[test]
    fn complete_sends_the_python_payload_and_cleans_quotes() {
        let server = MockServer::start(vec![MockResponse::json(
            200,
            r#"{"response":"  \"'Hello.'\"  "}"#,
        )]);
        let text = llm(&server).complete("sys", "user text", None).unwrap();
        assert_eq!(text, "Hello.");
        let request = server.request(0);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/generate");
        assert_eq!(
            request.json(),
            json!({
                "model": "llama3.2:3b",
                "prompt": "sys\n\nuser text",
                "options": {"num_ctx": 4096, "num_predict": -1},
                "stream": false,
            })
        );
        assert!(request.header("authorization").is_none());
    }

    #[test]
    fn complete_without_system_and_with_max_tokens() {
        let server = MockServer::start(vec![MockResponse::json(200, r#"{"response":"ok"}"#)]);
        llm(&server).complete("", "only user", Some(32)).unwrap();
        let body = server.request(0).json();
        assert_eq!(body["prompt"], "only user");
        assert_eq!(body["options"]["num_predict"], 32);
    }

    #[test]
    fn empty_response_is_reported_unwrapped() {
        for body in [r#"{"response":"  \"\" "}"#, r#"{}"#] {
            let server = MockServer::start(vec![MockResponse::json(200, body)]);
            let err = llm(&server).complete("s", "u", None).unwrap_err();
            assert_eq!(err.0, "Ollama returned an empty response");
        }
    }

    #[test]
    fn complete_errors_are_wrapped() {
        let server = MockServer::start(vec![MockResponse::json(500, r#"{"error":"x"}"#)]);
        let err = llm(&server).complete("s", "u", None).unwrap_err().0;
        assert!(err.starts_with("Ollama completion failed: 500 Server Error: Internal Server Error for url: http://127.0.0.1:"), "{err}");
        assert!(err.ends_with("/api/generate"), "{err}");

        let server = MockServer::start(vec![MockResponse::json(200, "not json")]);
        let err = llm(&server).complete("s", "u", None).unwrap_err().0;
        assert!(err.starts_with("Ollama completion failed: "), "{err}");

        let server = MockServer::start(vec![MockResponse::json(200, r#"{"response":5}"#)]);
        assert_eq!(
            llm(&server).complete("s", "u", None).unwrap_err().0,
            "Ollama completion failed: 'int' object has no attribute 'strip'"
        );

        let server = MockServer::start(vec![MockResponse::json(200, "[]")]);
        assert_eq!(
            llm(&server).complete("s", "u", None).unwrap_err().0,
            "Ollama completion failed: 'list' object has no attribute 'get'"
        );
    }

    #[test]
    fn connection_refused_is_wrapped() {
        let url = crate::testutil::UNREACHABLE_URL;
        let err = OllamaLlm::new(url, "m", 2, 1)
            .complete("s", "u", None)
            .unwrap_err()
            .0;
        assert!(err.starts_with("Ollama completion failed: "), "{err}");
        assert!(!OllamaLlm::new(url, "m", 2, 1).healthcheck().ok);
    }

    #[test]
    fn timeout_is_enforced() {
        let server = MockServer::start(vec![
            MockResponse::json(200, r#"{"response":"late"}"#).delayed(3000)
        ]);
        let started = std::time::Instant::now();
        let err = OllamaLlm::new(&server.url(), "m", 1, 1)
            .complete("s", "u", None)
            .unwrap_err();
        assert!(err.0.starts_with("Ollama completion failed: "), "{}", err.0);
        assert!(started.elapsed() < std::time::Duration::from_millis(2900));
    }

    #[test]
    fn invalid_timeout_fails_like_requests() {
        let err = OllamaLlm::new("http://127.0.0.1:9", "m", 0, 1)
            .list_models()
            .unwrap_err()
            .0;
        assert!(
            err.starts_with("Ollama model listing failed: Attempted to set timeout to 0"),
            "{err}"
        );
    }

    #[test]
    fn list_models_reads_names() {
        let server = MockServer::start(vec![MockResponse::json(
            200,
            r#"{"models":[{"name":"a:1","size":3},{"name":"b"}]}"#,
        )]);
        let models = llm(&server).list_models().unwrap();
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "a:1".into(),
                    label: "a:1".into(),
                    installed: Some(true)
                },
                ModelInfo {
                    id: "b".into(),
                    label: "b".into(),
                    installed: Some(true)
                },
            ]
        );
        let request = server.request(0);
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("GET", "/api/tags")
        );
    }

    #[test]
    fn list_models_empty_and_malformed() {
        for body in [
            "{}",
            r#"{"models":[]}"#,
            r#"{"models":""}"#,
            r#"{"models":{}}"#,
        ] {
            let server = MockServer::start(vec![MockResponse::json(200, body)]);
            assert_eq!(llm(&server).list_models().unwrap(), vec![], "{body}");
        }
        let cases = [
            (
                r#"{"models":[{"id":"x"}]}"#,
                "Ollama model listing failed: 'name'",
            ),
            (
                r#"{"models":null}"#,
                "Ollama model listing failed: 'NoneType' object is not iterable",
            ),
            (
                r#"{"models":[[1]]}"#,
                "Ollama model listing failed: list indices must be integers or slices, not str",
            ),
            (
                "[1]",
                "Ollama model listing failed: 'list' object has no attribute 'get'",
            ),
        ];
        for (body, expected) in cases {
            let server = MockServer::start(vec![MockResponse::json(200, body)]);
            assert_eq!(llm(&server).list_models().unwrap_err().0, expected);
        }
        let server = MockServer::start(vec![MockResponse::json(404, "{}")]);
        let err = llm(&server).list_models().unwrap_err().0;
        assert!(
            err.starts_with("Ollama model listing failed: 404 Client Error: Not Found for url: "),
            "{err}"
        );
        let server = MockServer::start(vec![MockResponse::json(200, "{bad")]);
        assert!(llm(&server).list_models().is_err());
    }

    #[test]
    fn healthcheck_uses_tags_and_status_only() {
        let server = MockServer::start(vec![MockResponse::json(200, "not json at all")]);
        assert_eq!(llm(&server).healthcheck(), Health::ok());
        assert_eq!(server.request(0).path, "/api/tags");

        let server = MockServer::start(vec![MockResponse::json(503, "")]);
        let health = llm(&server).healthcheck();
        assert!(!health.ok);
        assert!(
            health
                .detail
                .starts_with("503 Server Error: Service Unavailable for url: "),
            "{}",
            health.detail
        );
    }

    #[test]
    fn trait_metadata() {
        let client = OllamaLlm::new("http://h:1///", "m", 120, 8192);
        assert_eq!(client.base_url, "http://h:1");
        assert_eq!(client.id(), "ollama");
        assert!(client.offline());
    }
    #[test]
    fn pull_streams_aggregated_layers_and_requires_success() {
        let server = crate::testutil::MockServer::start(vec![crate::testutil::MockResponse::json(200,
            "{\"status\":\"pulling\",\"digest\":\"a\",\"completed\":4,\"total\":4}\n{\"status\":\"pulling\",\"digest\":\"b\",\"completed\":6,\"total\":6}\n{\"status\":\"success\"}")]);
        let events = std::sync::Mutex::new(Vec::new());
        OllamaLlm::new(&server.url(), "tiny", 120, 8192)
            .pull(
                &|d, t, s| events.lock().unwrap().push((d, t, s.to_owned())),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (4, 4, "pulling".into()),
                (10, 10, "pulling".into()),
                (10, 10, "success".into())
            ]
        );
        assert_eq!(server.request(0).path, "/api/pull");
        assert_eq!(
            server.request(0).json(),
            serde_json::json!({"model":"tiny","stream":true})
        );
    }

    #[test]
    fn pull_reports_http_protocol_empty_and_remote_errors() {
        for (code, body, expected) in [
            (500, "model unavailable", "model unavailable"),
            (200, "", "without success"),
            (200, "not json\n", "Invalid Ollama"),
            (200, "{}\n", "Missing Ollama"),
            (200, "{\"error\":\"disk full\"}\n", "disk full"),
            (200, "{\"status\":\"pulling\"}\n", "without success"),
        ] {
            let server =
                crate::testutil::MockServer::start(vec![crate::testutil::MockResponse::json(
                    code, body,
                )]);
            let error = OllamaLlm::new(&server.url(), "tiny", 120, 8192)
                .pull(&|_, _, _| {}, &std::sync::atomic::AtomicBool::new(false))
                .unwrap_err();
            assert!(error.0.contains(expected), "{error}");
        }
        let error = OllamaLlm::new(crate::testutil::UNREACHABLE_URL, "tiny", 120, 8192)
            .pull(&|_, _, _| {}, &std::sync::atomic::AtomicBool::new(false))
            .unwrap_err();
        assert!(error.0.contains("Ollama not running"), "{error}");
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        assert!(
            OllamaLlm::new(crate::testutil::UNREACHABLE_URL, "tiny", 120, 8192)
                .pull(&|_, _, _| panic!("cancelled"), &cancelled)
                .unwrap_err()
                .0
                .contains("cancelled")
        );
    }

    #[test]
    fn pull_cancel_closes_stalled_headers_and_body() {
        use std::io::{BufRead, Read, Write};
        for headers in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (ready, entered) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                if headers {
                    reader
                        .get_mut()
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
                        .unwrap();
                }
                ready.send(()).unwrap();
                assert!(matches!(reader.read(&mut [0]), Ok(0)));
            });
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop = cancel.clone();
            let (result, done) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                result
                    .send(OllamaLlm::new(&url, "tiny", 120, 8192).pull(&|_, _, _| {}, &stop))
                    .unwrap()
            });
            entered
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            cancel.store(true, std::sync::atomic::Ordering::Release);
            assert!(done
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .0
                .contains("cancelled"));
            worker.join().unwrap();
            server.join().unwrap();
        }
    }
}
