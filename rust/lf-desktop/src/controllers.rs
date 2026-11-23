//! Portable page logic. No GPUI types here, so all of it is unit-tested.

pub mod ask;
pub mod ask_agent;
pub mod assistant;
pub mod capabilities;
pub mod dictionary;
pub mod engine;
pub mod history;
pub mod instance;
pub mod jobs;
pub mod key_import;
pub mod mcp_client;
pub mod meetings;
pub mod model_storage;
pub mod models;
pub mod prompts;
pub mod provider_ops;
pub mod runtime_state;
pub mod settings;
pub mod setup;
pub mod setup_flow;
pub mod shutdown;
pub mod status;
