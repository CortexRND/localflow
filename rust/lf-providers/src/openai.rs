//! Port of `localflow/providers/llm_openai_compat.py`.

use crate::base::{Health, LlmProvider, ModelInfo, ProviderError};
use crate::http;
use serde_json::{json, Value};

#[derive(Clone, PartialEq)]
pub struct OpenAiCompatLlm {
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub timeout: i64,
    pub temperature: Option<f64>,
}

/// Never prints the key.
impl std::fmt::Debug for OpenAiCompatLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatLlm")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("has_api_key", &!self.api_key.is_empty())
            .field("timeout", &self.timeout)
            .field("temperature", &self.temperature)
            .finish()
    }
}

pub const DEFAULT_TIMEOUT: i64 = 120;

/// OpenAI's o-series reasoning models (`o1`, `o3`, `o4-mini`, dated or
/// variant IDs such as `o1-2024-12-17` or `o3-mini`) reject `max_tokens` on
/// `/chat/completions` and take the same bound as `max_completion_tokens`.
/// Matched on the model id: `o`, one or more digits, then the end or `-`.
/// Every other model (GPT-4o, Llama, provider-prefixed ids such as
/// OpenRouter's `openai/o4-mini`, which translates the field itself) keeps
/// `max_tokens`.
pub fn uses_max_completion_tokens(model: &str) -> bool {
    let id = model.trim().to_ascii_lowercase();
    let Some(rest) = id.strip_prefix('o') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && matches!(rest.as_bytes().get(digits), None | Some(&b'-'))
}

/// Total `max_completion_tokens` a reasoning model gets when a caller asks
/// for a reply budget ([`LlmProvider::reply_budget`]): the bound also covers
/// invisible reasoning tokens, so a visible-sized cap can be used up before
/// any reply text is written.
pub const REASONING_REPLY_BUDGET: i64 = 4096;

/// Whether `model` is an o-series reasoning model for the reply budget:
/// the same id rule as [`uses_max_completion_tokens`], after one `vendor/`
/// prefix (OpenRouter's `openai/o4-mini`). Only the budget uses this; the
/// wire field for a prefixed id stays `max_tokens`, which such routers map
/// themselves, and the bound still covers the model's hidden reasoning.
pub fn is_reasoning_model(model: &str) -> bool {
    let id = model.trim();
    let id = id.split_once('/').map_or(id, |(_, rest)| rest);
    uses_max_completion_tokens(id)
}

impl OpenAiCompatLlm {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: &str,
        timeout: i64,
        temperature: Option<f64>,
    ) -> Self {
        OpenAiCompatLlm {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            timeout,
            temperature,
        }
    }

    pub fn has_api_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// The `/chat/completions` request body.
    pub fn chat_body(&self, system: &str, user: &str, max_tokens: Option<i64>) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        // No bound sends neither field (basic recall's uncapped call).
        if let Some(max_tokens) = max_tokens {
            let field = if uses_max_completion_tokens(&self.model) {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[field] = json!(max_tokens);
        }
        if let Some(temperature) = self.temperature {
            body["temperature"] = json!(temperature);
        }
        body
    }

    fn authorize(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if self.api_key.is_empty() {
            request
        } else {
            request.header("Authorization", format!("Bearer {}", self.api_key))
        }
    }

    fn try_complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, String> {
        let client = http::client(self.timeout)?;
        let body = self.chat_body(system, user, max_tokens);
        let request = client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body);
        let data = http::send_json(self.authorize(request))?;
        let choices = http::dict_index(&data, "choices")?;
        let first = first_item(choices)?;
        let message = http::dict_index(first, "message")?;
        let content = http::dict_index(message, "content")?;
        Ok(crate::pyfmt::py_strip(http::as_str(content, "strip")?).to_string())
    }

    fn try_list(&self) -> Result<Vec<ModelInfo>, String> {
        let client = http::client(self.timeout)?;
        let request = client.get(format!("{}/models", self.base_url));
        let data = http::send_json(self.authorize(request))?;
        let empty = Value::Array(Vec::new());
        let models = http::dict_get(&data, "data")?.unwrap_or(&empty);
        http::iter_items(models)?
            .into_iter()
            .map(|model| {
                let id = http::dict_index(model, "id")?;
                let id = id
                    .as_str()
                    .ok_or_else(|| format!("model id is a {}, not a string", http::py_type(id)))?;
                Ok(ModelInfo {
                    id: id.into(),
                    label: id.into(),
                    installed: None,
                })
            })
            .collect()
    }
}

/// `value[0]` with Python's error text.
fn first_item(value: &Value) -> Result<&Value, String> {
    match value {
        Value::Array(items) => items
            .first()
            .ok_or_else(|| "list index out of range".to_string()),
        Value::Object(_) => Err("0".into()),
        Value::String(s) if s.is_empty() => Err("string index out of range".into()),
        Value::String(_) => Err("string indices must be integers, not 'str'".into()),
        other => Err(format!(
            "'{}' object is not subscriptable",
            http::py_type(other)
        )),
    }
}

impl LlmProvider for OpenAiCompatLlm {
    fn id(&self) -> &'static str {
        "openai-compatible"
    }

    fn offline(&self) -> bool {
        false
    }

    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError> {
        match self.try_complete(system, user, max_tokens) {
            Ok(text) if text.is_empty() => Err(ProviderError(
                "OpenAI-compatible provider returned an empty response".into(),
            )),
            Ok(text) => Ok(text),
            Err(exc) => Err(ProviderError(format!(
                "OpenAI-compatible completion failed: {exc}"
            ))),
        }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.try_list()
            .map_err(|exc| ProviderError(format!("OpenAI-compatible model listing failed: {exc}")))
    }

    fn healthcheck(&self) -> Health {
        match self.list_models() {
            Ok(_) => Health::ok(),
            Err(exc) => Health::failed(exc.to_string()),
        }
    }

    fn reply_budget(&self, visible: i64) -> i64 {
        if is_reasoning_model(&self.model) {
            visible.max(REASONING_REPLY_BUDGET)
        } else {
            visible
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockResponse, MockServer};

    fn llm(server: &MockServer, key: &str, temperature: Option<f64>) -> OpenAiCompatLlm {
        OpenAiCompatLlm::new(
            &format!("{}/v1/", server.url()),
            "gpt-x",
            key,
            5,
            temperature,
        )
    }

    const OK_BODY: &str =
        r#"{"choices":[{"message":{"role":"assistant","content":"  Hi there \n"}}]}"#;

    #[test]
    fn complete_sends_payload_and_bearer_key() {
        let server = MockServer::start(vec![MockResponse::json(200, OK_BODY)]);
        let text = llm(&server, "sk-test", None)
            .complete("sys", "usr", None)
            .unwrap();
        assert_eq!(text, "Hi there");
        let request = server.request(0);
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/v1/chat/completions")
        );
        assert_eq!(
            request.header("authorization").as_deref(),
            Some("Bearer sk-test")
        );
        assert_eq!(
            request.header("content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            request.json(),
            json!({"model": "gpt-x", "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "usr"},
            ]})
        );
    }

    #[test]
    fn complete_optional_fields_and_missing_token() {
        let server = MockServer::start(vec![MockResponse::json(200, OK_BODY)]);
        llm(&server, "", Some(0.3))
            .complete("s", "u", Some(7))
            .unwrap();
        let request = server.request(0);
        assert!(
            request.header("authorization").is_none(),
            "no key => no Authorization header"
        );
        let body = request.json();
        assert_eq!(body["max_tokens"], 7);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["temperature"], 0.3);
    }

    #[test]
    fn o_series_bound_is_sent_as_max_completion_tokens() {
        let server = MockServer::start(vec![MockResponse::json(200, OK_BODY)]);
        OpenAiCompatLlm::new(&format!("{}/v1", server.url()), "o4-mini", "k", 5, None)
            .complete("sys", "usr", Some(800))
            .unwrap();
        let body = server.request(0).json();
        assert_eq!(body["max_completion_tokens"], 800);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["model"], "o4-mini");
        assert_eq!(body["messages"][0]["role"], "system");
        // No bound: neither field (basic recall stays uncapped).
        let llm = OpenAiCompatLlm::new("https://x/v1", "o4-mini", "", 5, None);
        let body = llm.chat_body("s", "u", None);
        assert!(body.get("max_tokens").is_none() && body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn reply_budget_is_opt_in_and_reasoning_aware() {
        let reasoning = OpenAiCompatLlm::new("https://x/v1", "o4-mini", "", 5, None);
        let plain = OpenAiCompatLlm::new("https://x/v1", "gpt-4o-mini", "", 5, None);
        assert_eq!(reasoning.reply_budget(800), REASONING_REPLY_BUDGET);
        assert_eq!(reasoning.reply_budget(10_000), 10_000, "never lowered");
        assert_eq!(plain.reply_budget(800), 800);
        // Opted in: the larger allowance reaches the wire.
        let server = MockServer::start(vec![
            MockResponse::json(200, OK_BODY),
            MockResponse::json(200, OK_BODY),
            MockResponse::json(200, OK_BODY),
        ]);
        let url = format!("{}/v1", server.url());
        let o4 = OpenAiCompatLlm::new(&url, "o4-mini", "k", 5, None);
        o4.complete("s", "u", Some(o4.reply_budget(800))).unwrap();
        assert_eq!(
            server.request(0).json()["max_completion_tokens"],
            REASONING_REPLY_BUDGET
        );
        // A direct explicit bound is sent unchanged.
        o4.complete("s", "u", Some(800)).unwrap();
        assert_eq!(server.request(1).json()["max_completion_tokens"], 800);
        let gpt = OpenAiCompatLlm::new(&url, "gpt-4o-mini", "k", 5, None);
        gpt.complete("s", "u", Some(gpt.reply_budget(800))).unwrap();
        assert_eq!(server.request(2).json()["max_tokens"], 800);
    }

    #[test]
    fn prefixed_reasoning_ids_get_the_budget_but_keep_max_tokens() {
        let reasoning = [
            "openai/o4-mini",
            "openai/o1",
            "openai/o1-mini",
            "openai/o3-mini",
            "openai/o3-mini-2025-01-31",
            "o1-preview",
            "o3",
        ];
        let plain = [
            "openai/gpt-4o-mini",
            "openai/gpt-4.1",
            "meta-llama/llama-3.3-70b-instruct",
            "openai/omni-moderation-latest",
            "openai/o",
            "gpt-4o-mini",
        ];
        let server = MockServer::start(
            (0..reasoning.len() + plain.len() + 1)
                .map(|_| MockResponse::json(200, OK_BODY))
                .collect(),
        );
        let url = format!("{}/v1", server.url());
        let mut n = 0;
        for model in reasoning {
            let llm = OpenAiCompatLlm::new(&url, model, "k", 5, None);
            assert_eq!(llm.reply_budget(800), REASONING_REPLY_BUDGET, "{model}");
            llm.complete("s", "u", Some(llm.reply_budget(800))).unwrap();
            let body = server.request(n).json();
            n += 1;
            // Prefixed ids keep the router's `max_tokens`; bare ids use the
            // OpenAI field. Either way the reasoning allowance is sent.
            let (field, other) = if model.contains('/') {
                ("max_tokens", "max_completion_tokens")
            } else {
                ("max_completion_tokens", "max_tokens")
            };
            assert_eq!(body[field], REASONING_REPLY_BUDGET, "{model}");
            assert!(body.get(other).is_none(), "{model}");
        }
        for model in plain {
            let llm = OpenAiCompatLlm::new(&url, model, "k", 5, None);
            assert_eq!(llm.reply_budget(800), 800, "{model}");
            llm.complete("s", "u", Some(llm.reply_budget(800))).unwrap();
            let body = server.request(n).json();
            n += 1;
            assert_eq!(body["max_tokens"], 800, "{model}");
            assert!(body.get("max_completion_tokens").is_none(), "{model}");
        }
        // A direct explicit bound on a prefixed reasoning id is unchanged.
        let prefixed = OpenAiCompatLlm::new(&url, "openai/o4-mini", "k", 5, None);
        prefixed.complete("s", "u", Some(800)).unwrap();
        let body = server.request(n).json();
        assert_eq!(body["max_tokens"], 800);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn compatible_models_keep_max_tokens() {
        for model in [
            "gpt-4o-mini",
            "gpt-4.1",
            "llama-3.3-70b-versatile",
            "openai/o4-mini",
            "omni-moderation-latest",
            "o",
            "o-mini",
            "o4mini",
            "",
        ] {
            let llm = OpenAiCompatLlm::new("https://x/v1", model, "", 5, None);
            let body = llm.chat_body("s", "u", Some(7));
            assert_eq!(body["max_tokens"], 7, "{model}");
            assert!(body.get("max_completion_tokens").is_none(), "{model}");
        }
        for model in [
            "o1",
            "o1-mini",
            "o1-preview",
            "o1-2024-12-17",
            "o3",
            "o3-mini",
            "o3-mini-2025-01-31",
            "o4-mini",
            "O4-Mini-2025-04-16",
            " o4-mini ",
        ] {
            assert!(uses_max_completion_tokens(model), "{model}");
        }
    }

    #[test]
    fn complete_errors() {
        let cases = [
            (r#"{"choices":[{"message":{"content":"   "}}]}"#, "OpenAI-compatible provider returned an empty response"),
            (r#"{"choices":[]}"#, "OpenAI-compatible completion failed: list index out of range"),
            (r#"{}"#, "OpenAI-compatible completion failed: 'choices'"),
            (r#"{"choices":[{"message":{"content":null}}]}"#, "OpenAI-compatible completion failed: 'NoneType' object has no attribute 'strip'"),
            (r#"{"choices":[{"message":{}}]}"#, "OpenAI-compatible completion failed: 'content'"),
            (r#"[]"#, "OpenAI-compatible completion failed: list indices must be integers or slices, not str"),
            (r#"{"choices":{}}"#, "OpenAI-compatible completion failed: 0"),
        ];
        for (body, expected) in cases {
            let server = MockServer::start(vec![MockResponse::json(200, body)]);
            assert_eq!(
                llm(&server, "k", None)
                    .complete("s", "u", None)
                    .unwrap_err()
                    .0,
                expected,
                "{body}"
            );
        }
        let server = MockServer::start(vec![MockResponse::json(200, "<html>")]);
        let err = llm(&server, "k", None)
            .complete("s", "u", None)
            .unwrap_err()
            .0;
        assert!(
            err.starts_with("OpenAI-compatible completion failed: "),
            "{err}"
        );

        let server = MockServer::start(vec![MockResponse::json(401, r#"{"error":"bad key"}"#)]);
        let err = llm(&server, "k", None)
            .complete("s", "u", None)
            .unwrap_err()
            .0;
        assert!(
            err.starts_with(
                "OpenAI-compatible completion failed: 401 Client Error: Unauthorized for url: "
            ),
            "{err}"
        );
        assert!(!err.contains("Bearer"), "the key never appears in errors");
    }

    #[test]
    fn list_models_and_healthcheck() {
        let server = MockServer::start(vec![MockResponse::json(
            200,
            r#"{"data":[{"id":"a"},{"id":"b","owned_by":"x"}]}"#,
        )]);
        let models = llm(&server, "sk", None).list_models().unwrap();
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "a".into(),
                    label: "a".into(),
                    installed: None
                },
                ModelInfo {
                    id: "b".into(),
                    label: "b".into(),
                    installed: None
                },
            ]
        );
        let request = server.request(0);
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("GET", "/v1/models")
        );
        assert_eq!(
            request.header("authorization").as_deref(),
            Some("Bearer sk")
        );

        let server = MockServer::start(vec![MockResponse::json(200, "{}")]);
        assert_eq!(llm(&server, "", None).list_models().unwrap(), vec![]);
        assert!(server.request(0).header("authorization").is_none());

        let server = MockServer::start(vec![MockResponse::json(200, r#"{"data":[{"name":"x"}]}"#)]);
        assert_eq!(
            llm(&server, "", None).list_models().unwrap_err().0,
            "OpenAI-compatible model listing failed: 'id'"
        );

        let server = MockServer::start(vec![MockResponse::json(200, r#"{"data":[]}"#)]);
        assert_eq!(llm(&server, "k", None).healthcheck(), Health::ok());

        let server = MockServer::start(vec![MockResponse::json(403, "{}")]);
        let health = llm(&server, "k", None).healthcheck();
        assert!(!health.ok);
        assert!(
            health.detail.starts_with(
                "OpenAI-compatible model listing failed: 403 Client Error: Forbidden for url: "
            ),
            "{}",
            health.detail
        );

        let server = MockServer::start(vec![MockResponse::json(200, "garbage")]);
        assert!(!llm(&server, "k", None).healthcheck().ok);
    }

    #[test]
    fn debug_never_prints_the_key() {
        let client = OpenAiCompatLlm::new("https://x/", "m", "sk-secret", 120, None);
        let debug = format!("{client:?}");
        assert!(!debug.contains("sk-secret"));
        assert_eq!(client.base_url, "https://x");
        assert_eq!(client.id(), "openai-compatible");
        assert!(!client.offline());
        assert!(client.has_api_key());
    }
}
