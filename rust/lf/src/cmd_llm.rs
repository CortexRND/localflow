//! `llm list|use|test` (localflow/cli.py `llm` group).

use anyhow::Result;
use lf_core::config;
use lf_providers::{api, registry, CoreSecrets};
use serde_json::{json, Value};

use crate::args::LlmCommand;
use crate::pyfmt;
use crate::{fail, outln, CliError, Io};

pub(crate) fn run(command: LlmCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        LlmCommand::List {
            provider,
            base_url,
            json,
        } => list(provider.as_deref(), base_url.as_deref(), json, io),
        LlmCommand::Use {
            provider,
            base_url,
            model,
        } => use_provider(&provider, base_url, model, io),
        LlmCommand::Test {
            provider,
            base_url,
            model,
        } => test(
            provider.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
            io,
        ),
    }
}

fn list(
    provider: Option<&str>,
    base_url: Option<&str>,
    as_json: bool,
    io: &mut Io<'_>,
) -> Result<()> {
    let loaded = config::load_config()?;
    match api::cli_list_models(&loaded, provider, base_url, &CoreSecrets) {
        Ok(models) => {
            if as_json {
                let models: Vec<Value> = models
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<std::result::Result<_, _>>()?;
                outln!(
                    io.out,
                    "{}",
                    pyfmt::json_dumps(&json!({"models": models, "error": null}))
                )
            } else {
                for model in &models {
                    outln!(io.out, "{}: {}", model.id, model.label)?;
                }
                Ok(())
            }
        }
        Err(error) => {
            if as_json {
                outln!(
                    io.out,
                    "{}",
                    pyfmt::json_dumps(&json!({"models": [], "error": error}))
                )?;
            } else {
                outln!(io.out, "error: {error}")?;
            }
            Err(CliError::Exit(1).into())
        }
    }
}

fn use_provider(
    provider: &str,
    base_url: Option<String>,
    model: Option<String>,
    io: &mut Io<'_>,
) -> Result<()> {
    if !registry::list_llm_ids().contains(&provider) {
        return fail(format!("unknown LLM provider: {provider}"));
    }
    let loaded = config::load_config()?;
    let mut values = serde_json::Map::new();
    values.insert("provider".into(), json!(provider));
    if let Some(base_url) = base_url {
        values.insert("base_url".into(), json!(base_url));
    }
    if let Some(model) = model {
        values.insert("model".into(), json!(model));
    }
    let updated = config::apply_config_update(&loaded, &json!({ "llm": values }))?;
    let saved = config::save_config(&updated, None)?;
    outln!(io.out, "saved {}", saved.display())
}

fn test(
    provider: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    io: &mut Io<'_>,
) -> Result<()> {
    let loaded = config::load_config()?;
    let result = api::test_llm(&loaded, provider, base_url, model, &CoreSecrets);
    if result.ok {
        let detail = if result.detail.is_empty() {
            "reachable"
        } else {
            result.detail.as_str()
        };
        return outln!(io.out, "ok {detail}");
    }
    outln!(io.out, "{}", result.detail)?;
    Err(CliError::Exit(1).into())
}
