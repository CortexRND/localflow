use super::*;
use crate::testutil::{
    config_with, default_config, MemConfigs, MemSecrets, MockResponse, MockServer, TestConfig,
};

const KEY_A: &str = "key-a-openai-1111";
const KEY_B: &str = "key-b-groq-2222";
const OPENAI_URL: &str = "https://api.openai.com/v1";
const GROQ_URL: &str = "https://api.groq.com/openai/v1";

fn keys<'a>(secrets: &'a MemSecrets, configs: &'a MemConfigs) -> ProviderKeys<'a, TestConfig> {
    ProviderKeys::new(secrets, configs)
}

fn cloud_a() -> TestConfig {
    config_with(&[
        ("llm_provider", "openai-compatible"),
        ("llm_base_url", OPENAI_URL),
        ("llm_model", "model-a"),
    ])
}

/// What a reader that loads the config right now would send, and where.
fn effective_pair(secrets: &MemSecrets, config: &TestConfig) -> (String, String) {
    (
        config.get_str("llm_base_url"),
        crate::factory::llm_api_key(config, secrets),
    )
}

/// Provider A active, B has a saved key; legacy (generic key) or activated.
fn on_cloud_a(activated: bool) -> (MemSecrets, MemConfigs) {
    let secrets = MemSecrets::with(&[("provider_key_openai", KEY_A), ("provider_key_groq", KEY_B)]);
    let configs = MemConfigs::new(default_config());
    if activated {
        keys(&secrets, &configs)
            .activate("openai", None, Some("model-a"), Some(cloud_a()))
            .unwrap();
    } else {
        secrets
            .map
            .borrow_mut()
            .insert("llm_api_key".into(), KEY_A.into());
        configs.save(&cloud_a()).unwrap();
    }
    assert_eq!(
        effective_pair(&secrets, &configs.current()),
        (OPENAI_URL.into(), KEY_A.into())
    );
    (secrets, configs)
}

fn is_invalid(err: &anyhow::Error, message: &str) -> bool {
    err.downcast_ref::<InvalidRequest>().map(|e| e.0.as_str()) == Some(message)
}

#[test]
fn presets_catalog() {
    let ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
    assert_eq!(
        ids,
        [
            "ollama",
            "fireworks",
            "openai",
            "openrouter",
            "groq",
            "together",
            "custom"
        ]
    );
    let ollama = get_preset("ollama").unwrap();
    assert!(!ollama.needs_key);
    assert_eq!(
        (ollama.llm_provider, ollama.base_url, ollama.default_model),
        ("ollama", "http://localhost:11434", "llama3.2:3b")
    );
    assert_eq!(
        get_preset("fireworks").unwrap().base_url,
        "https://api.fireworks.ai/inference/v1"
    );
    assert_eq!(
        get_preset("openrouter").unwrap().base_url,
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(
        get_preset("together").unwrap().base_url,
        "https://api.together.xyz/v1"
    );
    assert_eq!(get_preset("groq").unwrap().base_url, GROQ_URL);
    assert_eq!(
        get_preset("custom").unwrap().env_vars,
        ["OPENAI_COMPAT_API_KEY", "LLM_API_KEY"]
    );
    assert_eq!(
        get_preset("custom").unwrap().label,
        "Custom OpenAI-compatible"
    );
    for preset in &PRESETS[1..] {
        assert!(
            preset.needs_key
                && !preset.env_vars.is_empty()
                && preset.llm_provider == "openai-compatible"
        );
        assert_eq!(preset.default_model, "");
    }
    assert!(is_invalid(
        &get_preset("missing").unwrap_err(),
        "Unknown inference provider"
    ));
    assert_eq!(
        provider_secret_names(),
        [
            "provider_key_fireworks",
            "provider_key_openai",
            "provider_key_openrouter",
            "provider_key_groq",
            "provider_key_together",
            "provider_key_custom"
        ]
    );
    assert_eq!(FIREWORKS_DEFAULT_MODEL, "accounts/fireworks/models/kimi-k3");
}

#[test]
fn masking() {
    for (value, masked) in [
        ("", ""),
        ("a", "••••"),
        ("abcd", "••••"),
        ("secretabcd", "••••abcd"),
        ("ééééé", "••••éééé"),
    ] {
        assert_eq!(mask_key(value), masked);
    }
}

#[test]
fn parse_env_literals_only() {
    let parsed = parse_env(
        "\n# comment\nexport FIREWORKS_API_KEY=\"fire works\" # comment\nOPENAI_API_KEY='open#ai'\nGROQ_API_KEY=groq # comment\nLLM_API_KEY=$OTHER_KEY\nOPENROUTER_API_KEY=\"${OTHER_KEY}\"\nTOGETHER_API_KEY=$(command)\nBAD='unterminated\necho ignored\nEMPTY=\n",
    );
    let expected: BTreeMap<String, String> = [
        ("FIREWORKS_API_KEY", "fire works"),
        ("OPENAI_API_KEY", "open#ai"),
        ("GROQ_API_KEY", "groq"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(parsed, expected);
    assert!(parse_env("").is_empty());
    let more = parse_env(
        "  export   A = 'x'\r\nB=two words\nC=a;b\nD=`x`\nE=\"\"\nexportF=1\n1X=2\nG=ok\\ esc",
    );
    assert_eq!(more.get("A").map(String::as_str), Some("x"));
    assert_eq!(more.get("exportF").map(String::as_str), Some("1"));
    assert_eq!(more.get("G").map(String::as_str), Some("ok esc"));
    for missing in ["B", "C", "D", "E", "1X"] {
        assert!(!more.contains_key(missing), "{missing}");
    }
}

#[test]
fn discovery_and_confirmed_subset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".zshrc");
    std::fs::write(
        &path,
        "export OPENAI_API_KEY='file-secret-1234'\nGROQ_API_KEY=$OTHER\n",
    )
    .unwrap();
    let environ: BTreeMap<String, String> = [
        (
            "FIREWORKS_API_KEY".to_string(),
            " env-secret-5678 ".to_string(),
        ),
        ("TOGETHER_API_KEY".into(), "  ".into()),
        ("GROQ_API_KEY".into(), "a$b".into()),
    ]
    .into();
    let found = detect_imports(Some(&path), &environ).unwrap();
    let summary: Vec<(String, String)> = found
        .iter()
        .map(|c| (c.provider_id.clone(), c.masked_value()))
        .collect();
    assert_eq!(
        summary,
        [
            ("fireworks".to_string(), "••••5678".to_string()),
            ("openai".into(), "••••1234".into())
        ]
    );
    assert!(!format!("{found:?}").contains("env-secret"));
    assert_eq!(found[0].source, "environment: FIREWORKS_API_KEY");
    assert_eq!(
        found[1].source,
        format!("{}: OPENAI_API_KEY", path.display())
    );

    let secrets = MemSecrets::default();
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    k.save_imports(&found[1..]).unwrap();
    assert_eq!(
        secrets.snapshot(),
        [(
            "provider_key_openai".to_string(),
            "file-secret-1234".to_string()
        )]
        .into()
    );
    k.save_imports(&[]).unwrap();
    k.save_imports(&found[..1]).unwrap();
    assert_eq!(
        secrets.get("provider_key_fireworks"),
        "env-secret-5678",
        "stored stripped"
    );
    assert_eq!(secrets.get("fireworks_api_key"), "env-secret-5678");

    assert!(detect_imports(None, &BTreeMap::new()).unwrap().is_empty());
    let err = detect_imports(Some(&dir.path().join("missing")), &BTreeMap::new()).unwrap_err();
    assert_eq!(
        err.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    let binary = dir.path().join("bin");
    std::fs::write(&binary, [0xff, 0xfe]).unwrap();
    assert!(detect_imports(Some(&binary), &BTreeMap::new()).is_err());
}

#[test]
fn storage_status_removal_and_fireworks_alias() {
    let secrets = MemSecrets::default();
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    assert_eq!(
        k.key_status("fireworks").unwrap(),
        KeyStatus {
            has_key: false,
            masked_value: String::new()
        }
    );
    k.save_key("fireworks", "  secret-abcd ").unwrap();
    assert_eq!(
        secrets.snapshot(),
        [
            ("fireworks_api_key".to_string(), "secret-abcd".to_string()),
            ("provider_key_fireworks".into(), "secret-abcd".into())
        ]
        .into()
    );
    assert_eq!(
        k.key_status("fireworks").unwrap(),
        KeyStatus {
            has_key: true,
            masked_value: "••••abcd".into()
        }
    );
    k.remove_key("fireworks", None).unwrap();
    assert!(secrets.snapshot().is_empty());
    k.remove_key("ollama", None).unwrap();
    assert!(!k.key_status("ollama").unwrap().has_key);
    let message = "A non-empty key and a keyed provider are required";
    assert!(is_invalid(&k.save_key("openai", " ").unwrap_err(), message));
    assert!(is_invalid(
        &k.save_key("ollama", "secret").unwrap_err(),
        message
    ));
    assert!(is_invalid(
        &k.save_key("nope", "secret").unwrap_err(),
        "Unknown inference provider"
    ));
    assert!(k.key_status("nope").is_err());
    assert_eq!(
        serde_json::to_string(&k.key_status("ollama").unwrap()).unwrap(),
        r#"{"has_key":false,"masked_value":""}"#
    );
}

#[test]
fn secrets_failures_propagate() {
    let secrets = MemSecrets::default();
    let configs = MemConfigs::new(default_config());
    secrets.on_set(|_, _| anyhow::bail!("unavailable"));
    let k = keys(&secrets, &configs);
    assert_eq!(
        k.save_key("openai", "secret").unwrap_err().to_string(),
        "unavailable"
    );
    secrets
        .map
        .borrow_mut()
        .insert("provider_key_openai".into(), "secret".into());
    let err = k
        .activate("openai", None, Some("m"), Some(default_config()))
        .unwrap_err();
    assert_eq!(err.to_string(), "unavailable");
    assert_eq!(*configs.saves.borrow(), 0);

    // A keyring that refuses deleting a missing item fails remove_key, like Python's keyring.
    let strict = MemSecrets::default();
    strict.strict_delete.set(true);
    assert!(keys(&strict, &configs).remove_key("openai", None).is_err());
}

#[test]
fn activation_never_writes_keys_to_config() {
    let secrets = MemSecrets::default();
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    let key = "private-key-not-in-config-1234";
    k.save_key("fireworks", key).unwrap();
    let config = k
        .activate(
            "fireworks",
            None,
            Some("chosen-model"),
            Some(default_config()),
        )
        .unwrap();
    assert_eq!(config.get_str("llm_provider"), "openai-compatible");
    assert_eq!(
        config.get_str("llm_base_url"),
        "https://api.fireworks.ai/inference/v1"
    );
    assert_eq!(config.get_str("llm_model"), "chosen-model");
    let reference = config.get_str("llm_key_ref");
    assert!(activation::is_ref(&reference));
    assert_eq!(activation::ref_provider(&reference), Some("fireworks"));
    assert_eq!(
        activation::resolve(&reference, &config.get_str("llm_base_url"), |n| secrets
            .get(n)),
        key
    );
    assert_eq!(
        secrets.get(&reference),
        format!(r#"{{"base_url":"https://api.fireworks.ai/inference/v1","key":"{key}"}}"#)
    );
    assert_eq!(
        secrets.get(activation::INDEX_NAME),
        format!(r#"["{reference}"]"#)
    );
    assert_eq!(secrets.get("llm_api_key"), "");
    assert_eq!(configs.current(), config);
    assert!(!serde_json::to_string(&configs.current())
        .unwrap()
        .contains(key));

    let local = k.activate("ollama", None, None, Some(config)).unwrap();
    assert_eq!(local.get_str("llm_provider"), "ollama");
    assert_eq!(local.get_str("llm_base_url"), "http://localhost:11434");
    assert_eq!(local.get_str("llm_key_ref"), "", "Ollama stays keyless");
    assert_eq!(local.get_str("llm_model"), "chosen-model");
    assert_eq!(configs.current(), local);
}

#[test]
fn activate_loads_config_when_none_given() {
    let secrets = MemSecrets::with(&[("provider_key_groq", KEY_B)]);
    let configs = MemConfigs::new(config_with(&[("llm_model", "kept"), ("other", "x")]));
    let updated = keys(&secrets, &configs)
        .activate("groq", None, None, None)
        .unwrap();
    assert_eq!(updated.get_str("llm_model"), "kept");
    assert_eq!(
        updated.get_str("other"),
        "x",
        "unrelated fields are preserved"
    );
    assert_eq!(configs.current(), updated);
}

#[test]
fn custom_activation_and_validation() {
    let secrets = MemSecrets::default();
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    let err = k
        .activate("openai", None, Some("m"), Some(default_config()))
        .unwrap_err();
    assert!(is_invalid(&err, "Save a provider key before activating it"));
    k.save_key("custom", "secret-custom").unwrap();
    let url_message = "Use an HTTP(S) base URL without credentials, query, or fragment";
    for url in [
        "",
        "file:///key",
        "https://user:secret@example.org",
        "https://user@example.org",
        "https://host/?key=secret",
        "https://host/#frag",
        "ftp://host",
        "https://",
        "localhost:11434",
    ] {
        let err = k
            .activate("custom", Some(url), Some("m"), Some(default_config()))
            .unwrap_err();
        assert!(is_invalid(&err, url_message), "{url}: {err}");
    }
    assert!(is_invalid(
        &k.activate("custom", None, Some("m"), Some(default_config()))
            .unwrap_err(),
        url_message
    ));
    assert!(is_invalid(
        &k.activate(
            "custom",
            Some("http://[::1"),
            Some("m"),
            Some(default_config())
        )
        .unwrap_err(),
        "Invalid IPv6 URL"
    ));
    assert!(
        secrets.get(activation::INDEX_NAME).is_empty(),
        "nothing written on refusal"
    );
    let config = k
        .activate(
            "custom",
            Some(" https://inference.example/v1/ "),
            Some("m"),
            Some(default_config()),
        )
        .unwrap();
    assert_eq!(
        config.get_str("llm_base_url"),
        "https://inference.example/v1"
    );
    let err = k
        .activate(
            "openai",
            Some("https://other.example"),
            Some("m"),
            Some(default_config()),
        )
        .unwrap_err();
    assert!(is_invalid(
        &err,
        "Use the custom preset for a different base URL"
    ));
    assert!(preset_url(
        get_preset("openai").unwrap(),
        Some("https://api.openai.com/v1/")
    )
    .is_ok());
    assert!(preset_url(get_preset("ollama").unwrap(), Some("http://[::1]:11434")).is_err());
    assert_eq!(
        preset_url(get_preset("custom").unwrap(), Some("HTTP://[::1]:8080/v1")).unwrap(),
        "HTTP://[::1]:8080/v1"
    );
}

#[test]
fn model_choice_rules() {
    let secrets = MemSecrets::with(&[("provider_key_groq", KEY_B), ("provider_key_custom", "c")]);
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    let blank = config_with(&[("llm_model", "  ")]);
    for model in [Some(""), Some("   "), None] {
        let err = k
            .activate("groq", None, model, Some(blank.clone()))
            .unwrap_err();
        assert!(
            is_invalid(&err, "Choose a model before using this provider"),
            "{model:?}"
        );
        let err = k
            .activate(
                "custom",
                Some("https://x.example/v1"),
                model,
                Some(blank.clone()),
            )
            .unwrap_err();
        assert!(is_invalid(
            &err,
            "Choose a model before using this provider"
        ));
    }
    assert!(
        secrets.writes.borrow().is_empty() && *configs.saves.borrow() == 0,
        "refused before any write"
    );
    // Ollama falls back to its own default instead of saving a blank model.
    assert_eq!(
        k.activate("ollama", None, Some(" "), Some(blank.clone()))
            .unwrap()
            .get_str("llm_model"),
        "llama3.2:3b"
    );
    assert_eq!(
        k.activate("ollama", None, None, Some(blank))
            .unwrap()
            .get_str("llm_model"),
        "llama3.2:3b"
    );
    assert_eq!(
        k.activate("ollama", None, Some(" mine "), None)
            .unwrap()
            .get_str("llm_model"),
        "mine"
    );
    let padded = config_with(&[("llm_model", "  kept  ")]);
    assert_eq!(
        k.activate("groq", None, None, Some(padded))
            .unwrap()
            .get_str("llm_model"),
        "kept"
    );
    assert_eq!(
        k.activate("groq", None, Some("\tnew\n"), None)
            .unwrap()
            .get_str("llm_model"),
        "new"
    );
    // The missing key is still refused with a real model.
    let empty = MemSecrets::default();
    assert!(is_invalid(
        &keys(&empty, &configs)
            .activate("groq", None, Some("m"), None)
            .unwrap_err(),
        "Save a provider key before activating it"
    ));
}

#[test]
fn failed_config_save_keeps_old_pair_and_vault() {
    for activated in [false, true] {
        let (secrets, configs) = on_cloud_a(activated);
        let before = secrets.snapshot();
        let disk_before = configs.current();
        *configs.fail_save.borrow_mut() = true;
        let err = keys(&secrets, &configs)
            .activate("groq", None, Some("model-b"), None)
            .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(configs.current(), disk_before);
        assert_eq!(
            secrets.snapshot(),
            before,
            "only the new snapshot and index entry were undone"
        );
        assert_eq!(
            effective_pair(&secrets, &configs.current()),
            (OPENAI_URL.into(), KEY_A.into())
        );
    }
}

#[test]
fn failed_save_from_ollama_leaves_no_generic_key() {
    let secrets = MemSecrets::with(&[("provider_key_groq", KEY_B)]);
    let configs = MemConfigs::new(default_config());
    *configs.fail_save.borrow_mut() = true;
    assert!(keys(&secrets, &configs)
        .activate("groq", None, Some("model-b"), None)
        .is_err());
    assert_eq!(configs.current().get_str("llm_provider"), "ollama");
    assert_eq!(
        secrets.snapshot(),
        [("provider_key_groq".to_string(), KEY_B.to_string())].into()
    );
}

#[test]
fn write_order_is_index_then_snapshot_then_config() {
    let (secrets, configs) = on_cloud_a(true);
    secrets.writes.borrow_mut().clear();
    let updated = keys(&secrets, &configs)
        .activate("groq", None, Some("model-b"), None)
        .unwrap();
    let reference = updated.get_str("llm_key_ref");
    assert_eq!(
        *secrets.writes.borrow(),
        vec![
            format!("set {}", activation::INDEX_NAME),
            format!("set {reference}")
        ]
    );
    assert_eq!(
        effective_pair(&secrets, &configs.current()),
        (GROQ_URL.into(), KEY_B.into())
    );
}

#[test]
fn failed_snapshot_write_keeps_the_prior_pair() {
    for activated in [false, true] {
        let (secrets, configs) = on_cloud_a(activated);
        let before = secrets.snapshot();
        secrets.on_set(|name, _| {
            if activation::is_ref(name) {
                anyhow::bail!("locked")
            } else {
                Ok(())
            }
        });
        let err = keys(&secrets, &configs)
            .activate("groq", None, Some("model-b"), None)
            .unwrap_err();
        assert_eq!(err.to_string(), "locked");
        assert_eq!(secrets.snapshot(), before);
        assert_eq!(
            effective_pair(&secrets, &configs.current()),
            (OPENAI_URL.into(), KEY_A.into())
        );
    }
}

#[test]
fn keychain_unavailable_leaves_everything_untouched() {
    let (secrets, configs) = on_cloud_a(true);
    let before = secrets.snapshot();
    secrets.on_set(|_, _| anyhow::bail!("locked"));
    assert!(keys(&secrets, &configs)
        .activate("groq", None, Some("model-b"), None)
        .is_err());
    assert_eq!(configs.current().get_str("llm_base_url"), OPENAI_URL);
    assert_eq!(secrets.snapshot(), before);
}

#[test]
fn failed_cleanup_keeps_orphan_indexed_and_revocable() {
    let (secrets, configs) = on_cloud_a(true);
    *configs.fail_save.borrow_mut() = true;
    secrets.on_delete(|name| {
        if activation::is_ref(name) {
            anyhow::bail!("delete refused")
        } else {
            Ok(())
        }
    });
    let err = keys(&secrets, &configs)
        .activate("groq", None, Some("model-b"), None)
        .unwrap_err();
    assert_eq!(err.to_string(), "disk full", "the original error surfaces");
    let index = activation::read_index(|n| secrets.get(n));
    let orphan: Vec<&String> = index
        .iter()
        .filter(|r| activation::ref_provider(r) == Some("groq"))
        .collect();
    assert_eq!(orphan.len(), 1, "the orphan stays listed");
    assert_ne!(configs.current().get_str("llm_key_ref"), *orphan[0]);
    // Later removal revokes it.
    *configs.fail_save.borrow_mut() = false;
    *secrets.fail_delete.borrow_mut() = None;
    let orphan = orphan[0].clone();
    keys(&secrets, &configs).remove_key("groq", None).unwrap();
    assert_eq!(secrets.get(&orphan), "");
    assert!(!activation::read_index(|n| secrets.get(n)).contains(&orphan));
    assert_eq!(
        effective_pair(&secrets, &configs.current()),
        (OPENAI_URL.into(), KEY_A.into())
    );
}

#[test]
fn cached_config_stays_paired_after_activating_another() {
    for activated in [false, true] {
        let (secrets, configs) = on_cloud_a(activated);
        let cached = configs.current();
        let b = keys(&secrets, &configs)
            .activate("groq", None, Some("model-b"), None)
            .unwrap();
        assert_eq!(
            effective_pair(&secrets, &cached),
            (OPENAI_URL.into(), KEY_A.into())
        );
        assert_eq!(
            effective_pair(&secrets, &b),
            (GROQ_URL.into(), KEY_B.into())
        );
    }
}

#[test]
fn saving_a_replacement_key_needs_activation() {
    for activated in [false, true] {
        let (secrets, configs) = on_cloud_a(activated);
        let k = keys(&secrets, &configs);
        assert!(!k.key_needs_activation("openai", None).unwrap());
        assert!(k.key_needs_activation("groq", None).unwrap());
        assert!(!k.key_needs_activation("ollama", None).unwrap());
        assert!(
            !k.key_needs_activation("together", None).unwrap(),
            "missing saved key never offered"
        );
        k.save_key("openai", "rotated-key-9999").unwrap();
        assert_eq!(
            effective_pair(&secrets, &configs.current()),
            (OPENAI_URL.into(), KEY_A.into()),
            "not activated"
        );
        assert!(k
            .key_needs_activation("openai", Some(&configs.current()))
            .unwrap());
        k.activate("openai", None, None, None).unwrap();
        assert!(!k.key_needs_activation("openai", None).unwrap());
        assert_eq!(
            effective_pair(&secrets, &configs.current()),
            (OPENAI_URL.into(), "rotated-key-9999".into())
        );
    }
}

#[test]
fn removing_the_active_key_disables_every_client_using_it() {
    let (secrets, configs) = on_cloud_a(true);
    let cached = configs.current();
    keys(&secrets, &configs)
        .remove_key("openai", Some(&cached))
        .unwrap();
    assert_eq!(
        effective_pair(&secrets, &cached),
        (OPENAI_URL.into(), String::new())
    );
    assert_eq!(secrets.get("provider_key_openai"), "");
    assert_eq!(
        secrets.get(activation::INDEX_NAME),
        "",
        "empty index is deleted"
    );
    assert_eq!(secrets.get("provider_key_groq"), KEY_B);
}

#[test]
fn removal_uses_the_config_ref_missing_from_the_index() {
    let (secrets, configs) = on_cloud_a(true);
    let cached = configs.current();
    secrets.map.borrow_mut().remove(activation::INDEX_NAME);
    keys(&secrets, &configs)
        .remove_key("openai", Some(&cached))
        .unwrap();
    assert_eq!(effective_pair(&secrets, &cached).1, "");
}

#[test]
fn removing_another_providers_key_leaves_the_active_one() {
    let (secrets, configs) = on_cloud_a(true);
    let index = secrets.get(activation::INDEX_NAME);
    keys(&secrets, &configs)
        .remove_key("groq", Some(&configs.current()))
        .unwrap();
    assert_eq!(
        effective_pair(&secrets, &configs.current()),
        (OPENAI_URL.into(), KEY_A.into())
    );
    assert_eq!(secrets.get(activation::INDEX_NAME), index);
}

#[test]
fn reactivating_the_same_key_reuses_its_snapshot() {
    let (secrets, configs) = on_cloud_a(true);
    let first = configs.current().get_str("llm_key_ref");
    secrets.writes.borrow_mut().clear();
    let again = keys(&secrets, &configs)
        .activate("openai", None, Some("model-c"), None)
        .unwrap();
    assert_eq!(again.get_str("llm_key_ref"), first);
    assert_eq!(again.get_str("llm_model"), "model-c");
    assert!(secrets.writes.borrow().is_empty(), "no keyring write");
}

#[test]
fn snapshot_names_are_unique() {
    let (secrets, configs) = on_cloud_a(true);
    let k = keys(&secrets, &configs);
    let a = configs.current().get_str("llm_key_ref");
    let b = k
        .activate("groq", None, Some("model-b"), None)
        .unwrap()
        .get_str("llm_key_ref");
    k.save_key("openai", "new-a-key").unwrap();
    let c = k
        .activate("openai", None, Some("model-a"), None)
        .unwrap()
        .get_str("llm_key_ref");
    assert!(a != b && b != c && a != c);
    assert_eq!(
        activation::read_index(|n| secrets.get(n)),
        vec![a.clone(), b, c]
    );
    assert_eq!(
        secrets.get(&a),
        activation::encode(OPENAI_URL, KEY_A),
        "existing snapshot is immutable"
    );
}

#[test]
fn snapshot_never_sent_to_another_endpoint() {
    let (secrets, configs) = on_cloud_a(true);
    let mut moved = configs.current();
    moved.set_str("llm_base_url", GROQ_URL);
    assert_eq!(
        effective_pair(&secrets, &moved),
        (GROQ_URL.into(), String::new())
    );
    // Even with a generic key present, a bound config never falls back to it.
    secrets
        .map
        .borrow_mut()
        .insert("llm_api_key".into(), "GENERIC".into());
    assert_eq!(effective_pair(&secrets, &moved).1, "");
    moved.set_str("llm_key_ref", "llm_activation_invalid");
    assert_eq!(effective_pair(&secrets, &moved).1, "");
}

#[test]
fn fetch_models_and_test_use_the_presets_own_key() {
    let server = MockServer::start(vec![
        MockResponse::json(200, r#"{"data":[{"id":"model-one"}]}"#),
        MockResponse::json(200, r#"{"data":[]}"#),
        MockResponse::json(401, "{}"),
        MockResponse::json(500, "{}"),
    ]);
    let url = format!("{}/v1", server.url());
    let secrets = MemSecrets::with(&[
        ("provider_key_custom", "custom-secret"),
        ("llm_api_key", "another-provider"),
    ]);
    let configs = MemConfigs::new(default_config());
    let k = keys(&secrets, &configs);
    assert_eq!(
        k.fetch_models("custom", Some(&url)),
        vec![ModelInfo {
            id: "model-one".into(),
            label: "model-one".into(),
            installed: None
        }]
    );
    assert_eq!(
        server.request(0).header("authorization").as_deref(),
        Some("Bearer custom-secret")
    );
    assert_eq!(
        k.test_llm("custom", Some(&url), "m"),
        (true, TEST_OK.to_string())
    );
    assert_eq!(
        k.test_llm("custom", Some(&url), "m"),
        (false, TEST_FAILED.to_string())
    );
    assert!(
        k.fetch_models("custom", Some(&url)).is_empty(),
        "errors yield []"
    );
    // Ollama probes are keyless; the custom URL rule does not apply to the openai preset.
    assert!(
        k.fetch_models("ollama", Some(&server.url())).is_empty(),
        "non-preset URL refused for ollama"
    );
    assert_eq!(
        k.test_llm("openai", Some(&url), ""),
        (false, TEST_UNABLE.to_string())
    );
    assert_eq!(
        k.test_llm("openai", None, ""),
        (false, TEST_UNABLE.to_string()),
        "no saved key"
    );
    assert!(k.fetch_models("together", None).is_empty());
    assert!(k.fetch_models("nope", None).is_empty());
    assert_eq!(server.requests().len(), 4);
}
