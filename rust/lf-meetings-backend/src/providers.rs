use anyhow::Result;
use lf_core::config::Config;
use lf_meetings::{title_client, titles::TITLE_TIMEOUT_SECONDS};
use lf_providers::{
    factory::{self, BuildOptions},
    ollama::OllamaLlm,
    openai::OpenAiCompatLlm,
    CoreSecrets, Health, LlmProvider, ModelInfo, ProviderError,
};
use std::{sync::Arc, time::Duration};

pub type TitleLlm = Arc<dyn LlmProvider + Send + Sync>;
pub trait ProviderFactory: Send + Sync + 'static {
    fn llm(&self, config: &Config) -> Result<Box<dyn LlmProvider>>;
    fn title(&self, _config: &Config) -> Option<TitleLlm> {
        None
    }
    fn work_prompts(&self, _config: &Config) -> Option<Box<dyn LlmProvider>> {
        None
    }
}
pub struct ConfiguredProviders;
const MEETING_OPTIONS: BuildOptions = BuildOptions {
    timeout: Some(300),
    num_ctx: Some(8192),
    temperature: None,
    with_api_key: true,
};
pub(crate) struct FailedProvider(pub String);
impl LlmProvider for FailedProvider {
    fn id(&self) -> &'static str {
        "unavailable"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(&self, _: &str, _: &str, _: Option<i64>) -> Result<String, ProviderError> {
        Err(ProviderError(self.0.clone()))
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Err(ProviderError(self.0.clone()))
    }
    fn healthcheck(&self) -> Health {
        Health::failed(self.0.clone())
    }
}
impl ProviderFactory for ConfiguredProviders {
    fn llm(&self, config: &Config) -> Result<Box<dyn LlmProvider>> {
        Ok(factory::build_llm(config, &MEETING_OPTIONS, &CoreSecrets)?)
    }
    fn title(&self, config: &Config) -> Option<TitleLlm> {
        let fast = config.fast_model_config();
        let config = fast.as_ref().unwrap_or(config);
        let base = config.field_str("llm_base_url").unwrap_or("");
        let model = config.field_str("llm_model").unwrap_or("");
        let timeout = MEETING_OPTIONS.timeout.unwrap();
        let duration = Duration::from_secs_f64(TITLE_TIMEOUT_SECONDS);
        match config.field_str("llm_provider").unwrap_or("") {
            "ollama" => title_client(
                Some(&OllamaLlm::new(
                    base,
                    model,
                    timeout,
                    MEETING_OPTIONS.num_ctx.unwrap(),
                )),
                duration,
            )
            .map(|llm| llm as TitleLlm),
            "openai-compatible" => title_client(
                Some(&OpenAiCompatLlm::new(
                    base,
                    model,
                    &factory::llm_api_key(config, &CoreSecrets),
                    timeout,
                    None,
                )),
                duration,
            )
            .map(|llm| llm as TitleLlm),
            _ => None,
        }
    }
    fn work_prompts(&self, config: &Config) -> Option<Box<dyn LlmProvider>> {
        factory::build_workprompts_llm(config, &CoreSecrets)
            .map(|llm| Box::new(llm) as Box<dyn LlmProvider>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn meeting_limits_override_dictation_limits_for_summary_and_title() {
        for fast_model in ["", "  fast-model  "] {
            check_meeting_models(fast_model);
        }
    }

    fn check_meeting_models(fast_model: &str) {
        assert_eq!(MEETING_OPTIONS.timeout, Some(300));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.extend(byte);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break bytes.len();
                    }
                };
                let headers = String::from_utf8_lossy(&bytes);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse().unwrap())
                    })
                    .unwrap();
                bytes.resize(end + length, 0);
                stream.read_exact(&mut bytes[end..]).unwrap();
                bodies.push(serde_json::from_slice::<Value>(&bytes[end..]).unwrap());
                let response = r#"{"response":"ok"}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
            bodies
        });
        let mut config = Config::default();
        config.set_field("llm_provider", json!("ollama"));
        config.set_field("llm_base_url", json!(format!("http://{address}")));
        config.set_field("llm_model", json!("main-model"));
        config.set_field("llm_fast_model", json!(fast_model));
        config.set_field("cleanup_num_ctx", json!(2048));
        config.set_field("cleanup_timeout", json!(5));
        ConfiguredProviders
            .llm(&config)
            .unwrap()
            .complete("", "summary", None)
            .unwrap();
        ConfiguredProviders
            .title(&config)
            .unwrap()
            .complete("", "title", Some(24))
            .unwrap();
        let bodies = server.join().unwrap();
        for body in &bodies {
            assert_eq!(body["options"]["num_ctx"], 8192);
        }
        assert_eq!(bodies[0]["model"], "main-model");
        let title_model = if fast_model.is_empty() {
            "main-model"
        } else {
            fast_model.trim()
        };
        assert_eq!(bodies[1]["model"], title_model);
    }
}
