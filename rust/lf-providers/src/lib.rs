//! LocalFlow LLM providers: Ollama / OpenAI-compatible clients, the provider
//! preset catalog and keys, and endpoint-bound activation snapshots.
//!
//! Port of `localflow/providers/*` (LLM parts), `localflow/llm_activation.py`,
//! `localflow/provider_keys.py`, `localflow/cleanup.py` and the LLM/secret
//! helpers of `localflow/api.py`. Secret values are never logged.

pub mod activation;
pub mod api;
pub mod base;
pub mod cleanup;
pub mod factory;
mod http;
pub mod keys;
pub mod ollama;
pub mod openai;
pub mod pyfmt;
pub mod registry;
mod shlex;
pub mod store;
pub mod transfer;
mod urlcheck;

#[cfg(test)]
pub(crate) mod testutil;

pub use base::{Health, LlmProvider, ModelInfo, ProviderError};
pub use keys::{ProviderKeys, PRESETS};
pub use store::{ConfigStore, CoreConfigStore, CoreSecrets, LlmConfig, SecretStore};
