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
        Ok(factory::build_llm(
            config,
            &BuildOptions::default(),
            &CoreSecrets,
        )?)
    }
    fn title(&self, config: &Config) -> Option<TitleLlm> {
        let base = config.field_str("llm_base_url").unwrap_or("");
        let model = config.field_str("llm_model").unwrap_or("");
        let timeout = config.field_i64("cleanup_timeout").unwrap_or(120);
        let duration = Duration::from_secs_f64(TITLE_TIMEOUT_SECONDS);
        match config.field_str("llm_provider").unwrap_or("") {
            "ollama" => title_client(
                Some(&OllamaLlm::new(
                    base,
                    model,
                    timeout,
                    config.field_i64("cleanup_num_ctx").unwrap_or(8192),
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
