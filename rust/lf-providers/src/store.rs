//! Injection points: the keyring and the config file.
//!
//! Every operation in this crate takes a [`SecretStore`] (and, for activation,
//! a [`ConfigStore`]) so tests run against in-memory fakes and never touch
//! real credentials. [`CoreSecrets`] / [`CoreConfigStore`] delegate to
//! `lf_core`, which owns keyring service/account names (`"localflow"`, secret
//! name as the account) and the environment precedence
//! (`LOCALFLOW_<NAME>`, then legacy `FIREWORKS_API_KEY` for
//! `fireworks_api_key`). This crate never reads or writes environment secrets.

use std::collections::BTreeMap;

pub trait SecretStore {
    /// The value, or "" when unset/unavailable (Python `get_secret`).
    fn get(&self, name: &str) -> String;
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()>;
    fn delete(&self, name: &str) -> anyhow::Result<()>;
}

/// `lf_core::secrets`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreSecrets;

impl SecretStore for CoreSecrets {
    fn get(&self, name: &str) -> String {
        lf_core::secrets::get_secret(name)
    }

    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        lf_core::secrets::set_secret(name, value)
    }

    fn delete(&self, name: &str) -> anyhow::Result<()> {
        lf_core::secrets::delete_secret(name)
    }
}

/// The handful of flat config fields this crate reads and writes.
pub trait LlmConfig: Clone {
    /// A string field; "" when missing or not a string.
    fn get_str(&self, field: &str) -> String;
    fn set_str(&mut self, field: &str, value: &str);
    /// An integer field (`cleanup_timeout`, `cleanup_num_ctx`).
    fn get_int(&self, field: &str) -> Option<i64>;
}

fn map_get_str(map: &BTreeMap<String, serde_json::Value>, field: &str) -> String {
    map.get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn map_get_int(map: &BTreeMap<String, serde_json::Value>, field: &str) -> Option<i64> {
    map.get(field).and_then(|v| v.as_i64())
}

impl LlmConfig for BTreeMap<String, serde_json::Value> {
    fn get_str(&self, field: &str) -> String {
        map_get_str(self, field)
    }

    fn set_str(&mut self, field: &str, value: &str) {
        self.insert(
            field.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn get_int(&self, field: &str) -> Option<i64> {
        map_get_int(self, field)
    }
}

impl LlmConfig for lf_core::config::Config {
    fn get_str(&self, field: &str) -> String {
        map_get_str(&self.fields, field)
    }

    fn set_str(&mut self, field: &str, value: &str) {
        self.fields.insert(
            field.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn get_int(&self, field: &str) -> Option<i64> {
        map_get_int(&self.fields, field)
    }
}

pub trait ConfigStore<C> {
    fn load(&self) -> anyhow::Result<C>;
    /// Must replace the stored config atomically (readers see old or new).
    fn save(&self, config: &C) -> anyhow::Result<()>;
}

/// `lf_core::config::{load_config, save_config}` at the default path.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreConfigStore;

impl ConfigStore<lf_core::config::Config> for CoreConfigStore {
    fn load(&self) -> anyhow::Result<lf_core::config::Config> {
        lf_core::config::load_config()
    }

    fn save(&self, config: &lf_core::config::Config) -> anyhow::Result<()> {
        lf_core::config::save_config(config, None).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_config_access() {
        let mut config = BTreeMap::new();
        assert_eq!(config.get_str("llm_model"), "");
        config.set_str("llm_model", "m");
        config.insert("cleanup_timeout".into(), serde_json::json!(30));
        config.insert("bad".into(), serde_json::json!(3));
        assert_eq!(config.get_str("llm_model"), "m");
        assert_eq!(config.get_str("bad"), "");
        assert_eq!(config.get_int("cleanup_timeout"), Some(30));
        assert_eq!(config.get_int("llm_model"), None);
    }
}
