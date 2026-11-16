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
        let url = crate::testutil::closed_port_url();
        let err = OllamaLlm::new(&url, "m", 2, 1)
            .complete("s", "u", None)
            .unwrap_err()
            .0;
        assert!(err.starts_with("Ollama completion failed: "), "{err}");
        assert!(!OllamaLlm::new(&url, "m", 2, 1).healthcheck().ok);
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
}
