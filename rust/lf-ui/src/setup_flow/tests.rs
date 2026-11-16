use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn cfg(fields: &[(&str, Value)]) -> Config {
    let mut config = Config::defaults_for(false);
    for (key, value) in fields {
        config.set_field(key, value.clone());
    }
    config
}

fn linux(ram: f64) -> HardwareInfo {
    HardwareInfo::new("Linux", "x86_64", ram, false, 50.0)
}

fn platform() -> FakeSetupPlatform {
    FakeSetupPlatform::default()
}

fn no_agent() -> FakeLaunchAgent {
    FakeLaunchAgent::new("not installed")
}

// ------------------------------------------------------- initial_choices ---

#[test]
fn initial_choices_use_hardware_recommendations_before_setup() {
    let choices = initial_choices(&cfg(&[]), &linux(4.0), &platform(), &no_agent()).unwrap();
    assert_eq!(choices.stt_id, "fw-tiny");
    assert_eq!(choices.llm_id, None);
    assert_eq!(choices.hotkey, "alt_l");
    assert!(!choices.start_on_login);
}

#[test]
fn initial_choices_reuse_existing_config_after_setup() {
    let config = cfg(&[
        ("setup_version", json!(1)),
        ("model_size", json!("small")),
        ("stt_backend", json!("faster-whisper")),
        ("llm_model", json!("qwen2.5:3b")),
        ("input_device", json!("Studio Mic")),
        ("hotkey", json!("f19")),
        ("language", json!("fr")),
        ("paste_last_hotkey", json!("<ctrl>+<shift>+v")),
        ("command_hotkey", json!("f18")),
        ("polish_hotkey", json!("f16")),
        ("prompt_hotkey", json!("f17")),
        ("pause_media_while_dictating", json!(true)),
        ("max_recording_seconds", json!(600)),
    ]);
    let choices = initial_choices(&config, &linux(16.0), &platform(), &no_agent()).unwrap();
    assert_eq!(
        (
            choices.stt_id.as_str(),
            choices.llm_id.as_deref(),
            choices.input_device.as_str(),
            choices.hotkey.as_str(),
            choices.language.as_deref(),
            choices.start_on_login
        ),
        (
            "fw-small",
            Some("qwen2.5:3b"),
            "Studio Mic",
            "f19",
            Some("fr"),
            false
        )
    );
    assert_eq!(choices.paste_last_hotkey, "<ctrl>+<shift>+v");
    assert_eq!(choices.command_hotkey, "f18");
    assert_eq!(choices.polish_hotkey, "f16");
    assert_eq!(choices.prompt_hotkey, "f17");
    assert!(choices.pause_media_while_dictating);
    assert_eq!(choices.max_recording_seconds, 600);
}

#[test]
fn initial_choices_replaces_models_unavailable_on_current_hardware() {
    let config = cfg(&[
        ("setup_version", json!(1)),
        ("stt_backend", json!("mlx")),
        ("model_size", json!("small")),
    ]);
    let choices = initial_choices(&config, &linux(8.0), &platform(), &no_agent()).unwrap();
    assert_eq!(choices.stt_id, "fw-base");
}

#[test]
fn initial_choices_skips_ollama_recommendation_for_other_provider() {
    let mut p = platform();
    p.forbid_ollama_probe = true;
    let config = cfg(&[("llm_provider", json!("openai-compatible"))]);
    let choices = initial_choices(&config, &linux(8.0), &p, &no_agent()).unwrap();
    assert_eq!(choices.llm_id, None);
    assert_eq!(choices.llm_provider, "openai-compatible");
}

#[test]
fn initial_choices_skips_ollama_default_when_not_installed() {
    let choices = initial_choices(&cfg(&[]), &linux(16.0), &platform(), &no_agent()).unwrap();
    assert_eq!(choices.llm_id, None);
}

#[test]
fn initial_choices_uses_ollama_recommendation_when_installed() {
    let mut p = platform();
    p.ollama_installed = true;
    let choices = initial_choices(&cfg(&[]), &linux(16.0), &p, &no_agent()).unwrap();
    assert_eq!(choices.llm_id.as_deref(), Some("llama3.2:3b"));
}

#[test]
fn after_setup_llm_is_kept_only_for_enabled_ollama_catalog_models() {
    let base = [("setup_version", json!(1))];
    let with = |extra: &[(&str, Value)]| {
        let mut fields = base.to_vec();
        fields.extend(extra.iter().cloned());
        initial_choices(&cfg(&fields), &linux(16.0), &platform(), &no_agent())
            .unwrap()
            .llm_id
    };
    assert_eq!(with(&[]).as_deref(), Some("llama3.2:3b"));
    assert_eq!(with(&[("cleanup_enabled", json!(false))]), None);
    assert_eq!(with(&[("llm_model", json!("custom:model"))]), None);
    assert_eq!(with(&[("llm_provider", json!("openai-compatible"))]), None);
}

#[test]
fn start_on_login_follows_the_launch_agent_on_darwin_only() {
    let config = cfg(&[("setup_version", json!(1))]);
    let mac = HardwareInfo::new("Darwin", "x86_64", 16.0, false, 50.0);
    for (status, expected) in [
        ("running", true),
        ("loaded, not running", true),
        ("not installed", false),
    ] {
        let agent = FakeLaunchAgent::new(status);
        let choices = initial_choices(&config, &mac, &platform(), &agent).unwrap();
        assert_eq!(choices.start_on_login, expected, "{status}");
    }
    // A failing launch agent is "not on login", not an error.
    let broken = FakeLaunchAgent::failing("launchctl missing");
    assert!(
        !initial_choices(&config, &mac, &platform(), &broken)
            .unwrap()
            .start_on_login
    );
    // Other systems never ask it.
    let agent = FakeLaunchAgent::new("running");
    initial_choices(&config, &linux(16.0), &platform(), &agent).unwrap();
    assert!(agent.calls().is_empty());
}

#[test]
fn initial_choices_fail_clearly_when_no_speech_runtime_exists() {
    let mut p = platform();
    p.runtimes.clear();
    let err = initial_choices(&cfg(&[]), &linux(16.0), &p, &no_agent()).unwrap_err();
    assert!(err.0.contains("faster-whisper"));
}

// ------------------------------------------------------------ run_checks ---

#[test]
fn run_checks_reports_all_cross_platform_requirements() {
    let mut p = platform();
    p.commands = vec!["ffmpeg".into()];
    p.ollama_installed = true;
    p.ollama_running = Some(false);
    p.input_devices = Ok(vec!["USB mic".into()]);
    let checks = run_checks(&cfg(&[]), &p);
    assert_eq!(
        checks.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        vec![
            "ffmpeg",
            "ollama-installed",
            "ollama-running",
            "microphone-device"
        ]
    );
    assert_eq!(
        checks.iter().map(|c| c.ok).collect::<Vec<_>>(),
        vec![Some(true), Some(true), Some(false), Some(true)]
    );
    assert_eq!(checks[0].fix_url, "https://formulae.brew.sh/formula/ffmpeg");
    assert_eq!(checks[2].detail, "Not running");
    assert_eq!(checks[3].detail, "1 input device(s) found");
}

#[test]
fn run_checks_uses_provider_info_without_ollama_checks() {
    let mut p = platform();
    p.system = "Darwin".into();
    p.forbid_ollama_probe = true;
    let config = cfg(&[
        ("llm_provider", json!("openai-compatible")),
        ("llm_model", json!("gpt-local")),
    ]);
    let checks = run_checks(&config, &p);
    assert_eq!(
        checks.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        vec![
            "ffmpeg",
            "llm-provider",
            "microphone-device",
            "microphone",
            "accessibility",
            "input-monitoring"
        ]
    );
    assert_eq!(checks[0].detail, "Not found — run: brew install ffmpeg");
    assert_eq!(
        checks[1],
        Check::new(
            "llm-provider",
            "Cleanup model",
            None,
            "Using openai-compatible (gpt-local)",
            "Open settings",
            ""
        )
    );
    assert_eq!(checks[2].detail, "No input devices found");
    assert_eq!(checks[2].ok, Some(false));
}

#[test]
fn run_checks_reports_unlistable_devices_and_unprobed_ollama_as_unknown() {
    let mut p = platform();
    p.input_devices = Err("no audio backend".into());
    p.ollama_running = None;
    let checks = run_checks(&cfg(&[]), &p);
    let running = checks.iter().find(|c| c.key == "ollama-running").unwrap();
    assert_eq!((running.ok, running.detail.as_str()), (None, "Unknown"));
    let device = checks
        .iter()
        .find(|c| c.key == "microphone-device")
        .unwrap();
    assert_eq!(device.ok, None);
    assert!(device.detail.contains("no audio backend"));
}

#[test]
fn host_platform_never_claims_what_it_did_not_check() {
    let host = HostPlatform;
    let checks = run_checks(&cfg(&[("llm_provider", json!("x"))]), &host);
    let device = checks
        .iter()
        .find(|c| c.key == "microphone-device")
        .unwrap();
    assert_eq!(device.ok, None);
    assert_eq!(host.ollama_running("http://localhost:11434"), None);
    assert!(host.runtime_available("faster-whisper"));
    assert!(!host.runtime_available("unlinked-test-runtime"));
    #[cfg(unix)]
    {
        let hardware = host.detect_hardware().unwrap();
        assert!(hardware.ram_gb > 0.0);
        assert!(hardware.free_disk_gb > 0.0);
    }
    #[cfg(not(unix))]
    assert!(host.detect_hardware().is_err());
    assert_eq!(host.mic_authorization(), None);
    assert!(!host.which("definitely-not-a-real-command-xyz"));
}

#[test]
fn mac_permission_checks_map_status_like_python() {
    let cases = [
        (None, None, "Unknown (permission status unavailable)"),
        (Some(MicAuthorization::NotDetermined), None, "Unknown"),
        (Some(MicAuthorization::Authorized), Some(true), "Granted"),
        (Some(MicAuthorization::Denied), Some(false), "Not granted"),
        (
            Some(MicAuthorization::Restricted),
            Some(false),
            "Not granted",
        ),
    ];
    for (status, ok, detail) in cases {
        let check = microphone_check(status);
        assert_eq!((check.ok, check.detail.as_str()), (ok, detail));
        assert_eq!(check.key, "microphone");
        assert!(check.fix_url.ends_with("Privacy_Microphone"));
    }
    let mut p = platform();
    p.accessibility = Some(true);
    p.input_monitoring = Some(false);
    let checks = mac_permission_checks(&p);
    assert_eq!(checks[1].detail, "Granted");
    assert!(checks[1].fix_url.ends_with("Privacy_Accessibility"));
    assert_eq!(
        (checks[2].ok, checks[2].detail.as_str()),
        (Some(false), "Not granted")
    );
    assert!(checks[2].fix_url.ends_with("Privacy_ListenEvent"));
    p.accessibility = None;
    assert_eq!(mac_permission_checks(&p)[1].detail, "Unknown");
}

// ------------------------------------------------------------ models ---

#[test]
fn catalog_matches_the_python_models() {
    let models = stt_models();
    assert_eq!(models.len(), 14);
    assert_eq!(models[0].id, "fw-tiny");
    assert_eq!(models[0].repo, "Systran/faster-whisper-tiny");
    assert_eq!(models[0].size_mb, 78);
    let distil = stt_model_by_id("fw-distil-large-v3").unwrap();
    assert!(distil.english_only && !distil.apple_silicon_only);
    assert_eq!(distil.repo, "Systran/faster-distil-whisper-large-v3");
    assert_eq!(
        stt_model_by_id("fw-large-v3-turbo").unwrap().repo,
        "mobiuslabsgmbh/faster-whisper-large-v3-turbo"
    );
    let parakeet = stt_model_by_id("parakeet-v2").unwrap();
    assert_eq!(
        (parakeet.backend, parakeet.model_size, parakeet.size_mb),
        ("parakeet", "large-v3", 2472)
    );
    assert!(parakeet.english_only && parakeet.apple_silicon_only);
    assert_eq!(stt_model_by_id("mlx-large-v3-turbo").unwrap().size_mb, 1614);
    assert!(models.iter().all(|m| m.size_mb > 0 && !m.repo.is_empty()));
    assert_eq!(LLM_MODELS[1].id, "llama3.2:3b");
    assert_eq!(LLM_MODELS[1].min_ram_gb, 16);
    assert_eq!(LLM_MODELS.len(), 7);
    assert!(stt_model_by_id("nope").is_none());
}

#[test]
fn recommend_follows_ram_and_apple_silicon() {
    let mut p = platform();
    assert_eq!(recommend(&linux(4.0), &p), ("fw-tiny".to_string(), None));
    assert_eq!(
        recommend(&linux(8.0), &p),
        ("fw-base".to_string(), Some("llama3.2:1b".to_string()))
    );
    assert_eq!(
        recommend(&linux(16.0), &p),
        ("fw-base".to_string(), Some("llama3.2:3b".to_string()))
    );
    let mac = HardwareInfo::new("Darwin", "arm64", 16.0, true, 50.0);
    // Apple Silicon without the MLX runtime falls back to faster-whisper.
    assert_eq!(recommend(&mac, &p).0, "fw-base");
    p.runtimes.push("mlx-whisper".into());
    assert_eq!(recommend(&mac, &p).0, "mlx-small");
    let small = HardwareInfo::new("Darwin", "arm64", 8.0, true, 50.0);
    assert_eq!(recommend(&small, &p).0, "mlx-base");
}

#[test]
fn available_models_respect_hardware_and_runtime() {
    let mut p = platform();
    p.runtimes = vec!["faster-whisper".into(), "mlx-whisper".into()];
    let ids = |hw: &HardwareInfo| -> Vec<String> {
        available_stt_models(hw, &p)
            .into_iter()
            .map(|m| m.id)
            .collect()
    };
    assert_eq!(ids(&linux(8.0)).len(), 7);
    let mac = HardwareInfo::new("Darwin", "arm64", 8.0, true, 50.0);
    let all = ids(&mac);
    assert_eq!(all.len(), 7 + 6);
    assert!(all.contains(&"mlx-small".to_string()));
    assert!(
        !all.contains(&"parakeet-v2".to_string()),
        "parakeet runtime missing"
    );
}

#[test]
fn current_stt_id_maps_backends() {
    let mut p = platform();
    let id = |backend: &str, size: &str, p: &FakeSetupPlatform| {
        current_stt_id(
            &cfg(&[("stt_backend", json!(backend)), ("model_size", json!(size))]),
            p,
        )
    };
    assert_eq!(id("parakeet", "small", &p), "parakeet-v2");
    assert_eq!(id("mlx-whisper", "small", &p), "mlx-small");
    assert_eq!(id("mlx", "base", &p), "mlx-base");
    assert_eq!(id("faster-whisper", "medium", &p), "fw-medium");
    assert_eq!(id("something-else", "tiny", &p), "fw-tiny");
    assert_eq!(id("faster-whisper", "huge", &p), "fw-base");
    assert_eq!(id("auto", "small", &p), "fw-small");
    p.system = "Darwin".into();
    p.machine = "arm64".into();
    p.runtimes.push("mlx-whisper".into());
    assert_eq!(id("auto", "small", &p), "mlx-small");
    assert_eq!(id("auto", "large-v3-turbo", &p), "mlx-large-v3-turbo");
}

#[test]
fn stt_config_from_id_builds_the_provider_update_or_rejects_unknown_ids() {
    assert_eq!(
        stt_config_from_id("fw-small").unwrap(),
        json!({"stt": {"provider": "faster-whisper", "model": "small"}})
    );
    assert_eq!(
        stt_config_from_id("mlx-medium").unwrap(),
        json!({"stt": {"provider": "mlx-whisper", "model": "medium", "device": "auto"}})
    );
    assert_eq!(
        stt_config_from_id("parakeet-v2").unwrap()["stt"]["device"],
        "auto"
    );
    let err = stt_config_from_id("bogus").unwrap_err();
    assert_eq!(err.to_string(), "unknown speech model: bogus");
    assert_eq!(
        stt_config_from_id("").unwrap_err().0,
        "unknown speech model: "
    );
}

// ---------------------------------------------------------------- apply ---

fn memory_store(current: Config) -> MemoryConfigStore {
    MemoryConfigStore::new(current, "/tmp/never-written/config.toml")
}

#[test]
fn apply_writes_configuration_and_can_install_launchagent() {
    let store = memory_store(Config::defaults_for(false));
    let mut choices = SetupChoices::new_for(
        false,
        "fw-small",
        Some("qwen2.5:3b"),
        "USB mic",
        "f19",
        Some("en"),
        true,
    );
    choices.cleanup_level = "medium".into();
    choices.pause_media_while_dictating = true;
    choices.max_recording_seconds = 600;
    let mut p = platform();
    p.system = "Darwin".into();
    let agent = no_agent();
    let path = apply(&choices, &store, &p, &agent).unwrap();
    assert_eq!(path, PathBuf::from("/tmp/never-written/config.toml"));
    let saved = &store.saved()[0];
    assert_eq!(saved.field_str("stt_backend"), Some("faster-whisper"));
    assert_eq!(saved.field_str("model_size"), Some("small"));
    assert_eq!(saved.field_str("llm_model"), Some("qwen2.5:3b"));
    assert_eq!(saved.field_bool("cleanup_enabled"), Some(true));
    assert_eq!(saved.field_str("input_device"), Some("USB mic"));
    assert_eq!(saved.field_str("hotkey"), Some("f19"));
    assert_eq!(saved.field_str("language"), Some("en"));
    assert_eq!(saved.field_i64("setup_version"), Some(1));
    assert_eq!(saved.field_str("cleanup_level"), Some("medium"));
    assert_eq!(
        saved.field_str("paste_last_hotkey"),
        Config::defaults_for(false).field_str("paste_last_hotkey")
    );
    assert_eq!(saved.field_str("command_hotkey"), Some("alt_r"));
    assert_eq!(saved.field_bool("pause_media_while_dictating"), Some(true));
    assert_eq!(saved.field_i64("max_recording_seconds"), Some(600));
    assert_eq!(agent.calls(), vec!["install"]);
}

#[test]
fn apply_uninstalls_launchagent_when_startup_is_disabled() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    let mut p = platform();
    p.system = "Darwin".into();
    let agent = FakeLaunchAgent::new("loaded, not running");
    apply(&choices, &store, &p, &agent).unwrap();
    assert_eq!(agent.calls(), vec!["status", "uninstall"]);
}

#[test]
fn apply_does_not_uninstall_missing_launchagent() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    let mut p = platform();
    p.system = "Darwin".into();
    let agent = no_agent();
    apply(&choices, &store, &p, &agent).unwrap();
    assert_eq!(agent.calls(), vec!["status"]);
}

#[test]
fn apply_never_touches_the_launch_agent_off_macos() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, true);
    let agent = no_agent();
    apply(&choices, &store, &platform(), &agent).unwrap();
    assert!(agent.calls().is_empty());
}

#[test]
fn apply_disables_cleanup_when_no_model_is_selected() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    apply(&choices, &store, &platform(), &no_agent()).unwrap();
    let saved = &store.saved()[0];
    assert_eq!(saved.field_bool("cleanup_enabled"), Some(false));
    assert_eq!(
        saved.field_str("llm_model"),
        Config::defaults_for(false).field_str("llm_model")
    );
    assert_eq!(saved.field("language"), Some(&Value::Null));
}

#[test]
fn apply_keeps_non_ollama_provider_configuration() {
    let current = cfg(&[
        ("llm_provider", json!("openai-compatible")),
        ("llm_base_url", json!("https://example.test/v1")),
        ("llm_model", json!("configured-model")),
    ]);
    let store = memory_store(current);
    let mut choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    choices.llm_provider = "openai-compatible".into();
    apply(&choices, &store, &platform(), &no_agent()).unwrap();
    let saved = &store.saved()[0];
    assert_eq!(saved.field_str("llm_provider"), Some("openai-compatible"));
    assert_eq!(
        saved.field_str("llm_base_url"),
        Some("https://example.test/v1")
    );
    assert_eq!(saved.field_str("llm_model"), Some("configured-model"));
    assert_eq!(saved.field_bool("cleanup_enabled"), Some(true));
}

#[test]
fn apply_rejects_an_unknown_speech_model_without_saving() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "bogus", None, "", "alt_l", None, false);
    let err = apply(&choices, &store, &platform(), &no_agent()).unwrap_err();
    assert!(err.to_string().contains("unknown speech model: bogus"));
    assert!(store.saved().is_empty());
}

#[test]
fn apply_rejects_invalid_values_without_saving() {
    let store = memory_store(Config::defaults_for(false));
    let mut choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    choices.max_recording_seconds = -5;
    assert!(apply(&choices, &store, &platform(), &no_agent()).is_err());
    assert!(store.saved().is_empty());
}

#[test]
fn launch_agent_failures_surface_after_the_config_was_written() {
    let store = memory_store(Config::defaults_for(false));
    let choices = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    let mut p = platform();
    p.system = "Darwin".into();
    let err = apply(&choices, &store, &p, &FakeLaunchAgent::failing("launchctl")).unwrap_err();
    assert!(err.to_string().contains("launchctl"));
    assert_eq!(store.saved().len(), 1);
    let err = apply(&choices, &store, &p, &UnavailableLaunchAgent).unwrap_err();
    assert!(err.to_string().contains("not available"));
}

#[test]
fn file_store_round_trips_in_a_temp_dir_and_missing_file_loads_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("config.toml");
    let store = FileConfigStore::at(&path);
    assert_eq!(store.load().unwrap(), Config::default());
    let mut choices = SetupChoices::new_for(
        false,
        "fw-small",
        Some("llama3.2:1b"),
        "Mic é 日本",
        "f19",
        Some("fr"),
        false,
    );
    choices.cleanup_level = "medium".into();
    let written = apply(&choices, &store, &platform(), &no_agent()).unwrap();
    assert_eq!(written, path);
    let loaded = store.load().unwrap();
    assert_eq!(loaded.field_str("model_size"), Some("small"));
    assert_eq!(loaded.field_str("input_device"), Some("Mic é 日本"));
    assert_eq!(loaded.field_str("language"), Some("fr"));
    assert_eq!(loaded.field_i64("setup_version"), Some(1));
    assert_eq!(loaded.field_str("llm_model"), Some("llama3.2:1b"));
    assert_eq!(loaded.field_str("cleanup_level"), Some("medium"));
}

#[test]
fn setup_choices_defaults_match_python() {
    let mac = SetupChoices::new_for(true, "fw-base", None, "", "alt_l", None, false);
    assert_eq!(mac.paste_last_hotkey, "<ctrl>+<cmd>+v");
    let other = SetupChoices::new_for(false, "fw-base", None, "", "alt_l", None, false);
    assert_eq!(other.paste_last_hotkey, "<ctrl>+<shift>+v");
    assert_eq!(
        (
            other.llm_provider.as_str(),
            other.cleanup_level.as_str(),
            other.command_hotkey.as_str()
        ),
        ("ollama", "light", "alt_r")
    );
    assert_eq!(
        (other.polish_hotkey.as_str(), other.prompt_hotkey.as_str()),
        ("<ctrl>+<alt>+1", "<ctrl>+<alt>+2")
    );
    assert!(!other.pause_media_while_dictating);
    assert_eq!(other.max_recording_seconds, 1200);
    assert_eq!(HOTKEY_OPTIONS.len(), 6);
    assert_eq!(
        HOTKEY_OPTIONS[5],
        ("<cmd>+<shift>+<space>", "Press ⌘⇧Space to start/stop")
    );
}

// ---------------------------------------------------------------- audio ---

#[test]
fn block_level_is_the_capped_rms() {
    assert_eq!(block_level(&[]), 0.0);
    assert_eq!(block_level(&[0.0, 0.0]), 0.0);
    assert!((block_level(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-9);
    assert_eq!(block_level(&[3.0, -3.0]), 1.0);
}

struct FakeStream(Arc<AtomicUsize>);
impl MicInputStream for FakeStream {
    fn stop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeMic {
    opened: Mutex<Vec<(Option<String>, u32)>>,
    stops: Arc<AtomicUsize>,
    callback: Mutex<Option<BlockCallback>>,
    fail: bool,
}

impl MicInput for FakeMic {
    fn open(
        &self,
        device: Option<&str>,
        sample_rate: u32,
        on_block: BlockCallback,
    ) -> std::result::Result<Box<dyn MicInputStream>, String> {
        if self.fail {
            return Err("Permission denied".into());
        }
        self.opened
            .lock()
            .unwrap()
            .push((device.map(str::to_string), sample_rate));
        *self.callback.lock().unwrap() = Some(on_block);
        Ok(Box::new(FakeStream(self.stops.clone())))
    }
}

#[test]
fn mic_level_monitor_tracks_blocks_and_resets_on_stop() {
    let mic = FakeMic::default();
    let mut monitor = MicLevelMonitor::new(Some(""));
    assert_eq!(monitor.device, None);
    assert_eq!(monitor.level(), 0.0);
    monitor.start(&mic).unwrap();
    assert_eq!(mic.opened.lock().unwrap()[0], (None, 16000));
    let feed = |block: &[f32]| (mic.callback.lock().unwrap().as_ref().unwrap())(block);
    feed(&[0.25, -0.25]);
    assert!((monitor.level() - 0.25).abs() < 1e-9);
    feed(&[2.0]);
    assert_eq!(monitor.level(), 1.0);
    monitor.stop();
    assert_eq!(monitor.level(), 0.0);
    assert_eq!(mic.stops.load(Ordering::SeqCst), 1);
    monitor.stop();
    assert_eq!(mic.stops.load(Ordering::SeqCst), 1, "stop is idempotent");
}

#[test]
fn mic_level_monitor_reports_open_failures() {
    let mic = FakeMic {
        fail: true,
        ..FakeMic::default()
    };
    let mut monitor = MicLevelMonitor::new(Some("USB mic"));
    assert_eq!(monitor.device.as_deref(), Some("USB mic"));
    assert_eq!(monitor.start(&mic).unwrap_err(), "Permission denied");
    assert_eq!(monitor.level(), 0.0);
}

type Log = Arc<Mutex<Vec<String>>>;

struct FakeRecorder(Log);
impl AudioRecorder for FakeRecorder {
    fn start(&mut self) -> std::result::Result<(), String> {
        self.0.lock().unwrap().push("start".into());
        Ok(())
    }
    fn stop(&mut self) -> Vec<f32> {
        self.0.lock().unwrap().push("stop".into());
        vec![0.1, 0.2]
    }
}

struct FakeFactory(Log);
impl RecorderFactory for FakeFactory {
    fn create(&self, sample_rate: u32, device: Option<&str>) -> Box<dyn AudioRecorder> {
        self.0
            .lock()
            .unwrap()
            .push(format!("init {sample_rate} {device:?}"));
        Box::new(FakeRecorder(self.0.clone()))
    }
}

struct FakeTranscriber(Log);
impl Transcriber for FakeTranscriber {
    fn transcribe(&self, audio: &[f32]) -> std::result::Result<String, String> {
        self.0
            .lock()
            .unwrap()
            .push(format!("transcribe {}", audio.len()));
        Ok("hello".into())
    }
}

#[test]
fn try_dictation_records_and_transcribes() {
    let log: Log = Arc::default();
    let sleeps = Mutex::new(Vec::new());
    let result = try_dictation(
        0.0,
        Some("USB mic"),
        &FakeFactory(log.clone()),
        &FakeTranscriber(log.clone()),
        &|s| sleeps.lock().unwrap().push(s),
    );
    assert_eq!(result.unwrap(), "hello");
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "init 16000 Some(\"USB mic\")",
            "start",
            "stop",
            "transcribe 2"
        ]
    );
    assert_eq!(*sleeps.lock().unwrap(), vec![0.0]);
    // An empty device name means the default device.
    let log2: Log = Arc::default();
    try_dictation(
        1.0,
        Some(""),
        &FakeFactory(log2.clone()),
        &FakeTranscriber(log2.clone()),
        &|_| {},
    )
    .unwrap();
    assert_eq!(log2.lock().unwrap()[0], "init 16000 None");
}

#[test]
fn try_dictation_stops_the_recorder_even_if_waiting_panics() {
    let log: Log = Arc::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = try_dictation(
            1.0,
            None,
            &FakeFactory(log.clone()),
            &FakeTranscriber(log.clone()),
            &|_| panic!("interrupted"),
        );
    }));
    assert!(outcome.is_err());
    let entries = log.lock().unwrap().clone();
    assert_eq!(entries.last().map(String::as_str), Some("stop"));
    assert!(!entries.iter().any(|e| e.starts_with("transcribe")));
}
