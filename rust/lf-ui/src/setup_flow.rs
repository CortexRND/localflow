//! UI-independent onboarding choices, checks and local audio helpers (port of
//! `localflow/setup_flow.py`, plus the pure parts of `localflow/models.py` it
//! uses: the speech/cleanup model catalogs, hardware recommendation and the
//! STT config mapping).
//!
//! Everything that depends on the machine — executables on `PATH`, Ollama,
//! audio devices, macOS privacy permissions, the launch agent, RAM/disk, the
//! available speech runtimes — sits behind the [`SetupPlatform`],
//! [`LaunchAgent`], [`MicInput`] and [`RecorderFactory`] traits. [`HostPlatform`]
//! reports only what the standard library can establish and says "unknown" or
//! "unavailable" for the rest; it never claims a permission, device or
//! runtime it did not check. Tests use the in-memory fakes below, and no test
//! writes the real config or touches the keychain or network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use lf_core::config::{apply_config_update, load_config, load_from, save_config, Config};
use serde_json::{json, Map, Value};

// --------------------------------------------------------------- models ---

/// One speech-to-text model of the catalog (`models.SttModel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SttModel {
    pub id: String,
    pub label: &'static str,
    pub backend: &'static str,
    pub model_size: &'static str,
    pub repo: &'static str,
    pub size_mb: u32,
    pub english_only: bool,
    pub speed: u8,
    pub accuracy: u8,
    pub blurb: &'static str,
    pub apple_silicon_only: bool,
}

/// One cleanup (LLM) model of the catalog (`models.LlmModel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmModel {
    pub id: &'static str,
    pub label: &'static str,
    pub size_mb: u32,
    pub min_ram_gb: u32,
    pub blurb: &'static str,
}

/// Hugging Face sizes measured 2026-10-30 (`models._STT_SIZES_MB`).
pub const STT_SIZES_MB: [(&str, u32); 14] = [
    ("fw-tiny", 78),
    ("fw-base", 148),
    ("fw-small", 486),
    ("fw-medium", 1531),
    ("fw-large-v3-turbo", 1622),
    ("fw-large-v3", 3091),
    ("fw-distil-large-v3", 1516),
    ("mlx-tiny", 74),
    ("mlx-base", 144),
    ("mlx-small", 481),
    ("mlx-medium", 1525),
    ("mlx-large-v3", 3084),
    ("mlx-large-v3-turbo", 1614),
    ("parakeet-v2", 2472),
];

/// `(size, label, speed, accuracy, blurb)` of the faster-whisper catalog.
pub const FW_CATALOG: [(&str, &str, u8, u8, &str); 7] = [
    (
        "tiny",
        "Tiny",
        5,
        2,
        "Smallest download; best for quick tests.",
    ),
    (
        "base",
        "Base",
        5,
        3,
        "A balanced starting point for CPU transcription.",
    ),
    (
        "small",
        "Small",
        4,
        4,
        "More accurate while staying practical on most machines.",
    ),
    (
        "medium",
        "Medium",
        3,
        5,
        "High accuracy with a larger download.",
    ),
    (
        "large-v3-turbo",
        "Large v3 Turbo",
        5,
        5,
        "Near-large accuracy with faster decoding.",
    ),
    (
        "large-v3",
        "Large v3",
        2,
        5,
        "Maximum Whisper accuracy; the largest model.",
    ),
    (
        "distil-large-v3",
        "Distil Large v3",
        4,
        5,
        "Fast, English-only distillation of Large v3.",
    ),
];

/// `(size, label, speed, accuracy, blurb)` of the mlx-whisper catalog.
pub const MLX_CATALOG: [(&str, &str, u8, u8, &str); 5] = [
    (
        "tiny",
        "Tiny",
        5,
        2,
        "Smallest download; best for quick tests.",
    ),
    (
        "base",
        "Base",
        5,
        3,
        "A balanced Apple Silicon starting point.",
    ),
    (
        "small",
        "Small",
        4,
        4,
        "Recommended balance for Apple Silicon.",
    ),
    (
        "medium",
        "Medium",
        3,
        5,
        "High accuracy with a larger download.",
    ),
    (
        "large-v3",
        "Large v3",
        4,
        5,
        "Maximum Whisper accuracy on Apple Silicon.",
    ),
];

/// faster-whisper's `_MODELS` repositories for the sizes the catalog uses.
pub fn fw_repo(size: &str) -> Option<&'static str> {
    Some(match size {
        "tiny" => "Systran/faster-whisper-tiny",
        "base" => "Systran/faster-whisper-base",
        "small" => "Systran/faster-whisper-small",
        "medium" => "Systran/faster-whisper-medium",
        "large-v3" => "Systran/faster-whisper-large-v3",
        "distil-large-v3" => "Systran/faster-distil-whisper-large-v3",
        "large-v3-turbo" => "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
        _ => return None,
    })
}

/// `providers.stt_mlx_whisper._MLX_REPOS`.
pub fn mlx_repo(size: &str) -> Option<&'static str> {
    Some(match size {
        "tiny" => "mlx-community/whisper-tiny-mlx",
        "base" => "mlx-community/whisper-base-mlx",
        "small" => "mlx-community/whisper-small-mlx",
        "medium" => "mlx-community/whisper-medium-mlx",
        "large-v3" => "mlx-community/whisper-large-v3-mlx",
        "large-v3-turbo" => "mlx-community/whisper-large-v3-turbo",
        _ => return None,
    })
}

/// `providers.stt_parakeet.PARAKEET_REPO`.
pub const PARAKEET_REPO: &str = "mlx-community/parakeet-tdt-0.6b-v2";

fn size_mb(id: &str) -> u32 {
    STT_SIZES_MB
        .iter()
        .find(|(name, _)| *name == id)
        .map_or(0, |(_, size)| *size)
}

/// `models.STT_MODELS`, in catalog order.
pub fn stt_models() -> Vec<SttModel> {
    let mut models = Vec::new();
    for (size, label, speed, accuracy, blurb) in FW_CATALOG {
        let id = format!("fw-{size}");
        models.push(SttModel {
            size_mb: size_mb(&id),
            id,
            label,
            backend: "faster-whisper",
            model_size: size,
            repo: fw_repo(size).unwrap_or(""),
            english_only: size == "distil-large-v3",
            speed,
            accuracy,
            blurb,
            apple_silicon_only: false,
        });
    }
    for (size, label, speed, accuracy, blurb) in MLX_CATALOG {
        let id = format!("mlx-{size}");
        models.push(SttModel {
            size_mb: size_mb(&id),
            id,
            label,
            backend: "mlx-whisper",
            model_size: size,
            repo: mlx_repo(size).unwrap_or(""),
            english_only: false,
            speed,
            accuracy,
            blurb,
            apple_silicon_only: true,
        });
    }
    models.push(SttModel {
        id: "mlx-large-v3-turbo".to_string(),
        label: "Large v3 Turbo",
        backend: "mlx-whisper",
        model_size: "large-v3-turbo",
        repo: mlx_repo("large-v3-turbo").unwrap_or(""),
        size_mb: size_mb("mlx-large-v3-turbo"),
        english_only: false,
        speed: 5,
        accuracy: 5,
        blurb: "Near-large accuracy with fast Apple Silicon decoding.",
        apple_silicon_only: true,
    });
    models.push(SttModel {
        id: "parakeet-v2".to_string(),
        label: "Parakeet v2",
        backend: "parakeet",
        model_size: "large-v3",
        repo: PARAKEET_REPO,
        size_mb: size_mb("parakeet-v2"),
        english_only: true,
        speed: 5,
        accuracy: 5,
        blurb: "Fast English-only speech recognition.",
        apple_silicon_only: true,
    });
    models
}

/// Ollama manifest sizes measured 2026-10-30 (`models.LLM_MODELS`).
pub const LLM_MODELS: [LlmModel; 7] = [
    LlmModel {
        id: "llama3.2:1b",
        label: "Llama 3.2 · 1B",
        size_mb: 1321,
        min_ram_gb: 8,
        blurb: "The lightest cleanup option.",
    },
    LlmModel {
        id: "llama3.2:3b",
        label: "Llama 3.2 · 3B",
        size_mb: 2019,
        min_ram_gb: 16,
        blurb: "Strong cleanup in a compact model.",
    },
    LlmModel {
        id: "qwen2.5:3b",
        label: "Qwen 2.5 · 3B",
        size_mb: 1930,
        min_ram_gb: 8,
        blurb: "Compact multilingual cleanup.",
    },
    LlmModel {
        id: "qwen3:4b",
        label: "Qwen 3 · 4B",
        size_mb: 2497,
        min_ram_gb: 8,
        blurb: "A capable small reasoning model.",
    },
    LlmModel {
        id: "gemma3:4b",
        label: "Gemma 3 · 4B",
        size_mb: 3350,
        min_ram_gb: 8,
        blurb: "A polished general-purpose small model.",
    },
    LlmModel {
        id: "qwen2.5:7b",
        label: "Qwen 2.5 · 7B",
        size_mb: 4683,
        min_ram_gb: 16,
        blurb: "Higher quality, with a larger memory footprint.",
    },
    LlmModel {
        id: "llama3.1:8b",
        label: "Llama 3.1 · 8B",
        size_mb: 4921,
        min_ram_gb: 16,
        blurb: "A larger, capable cleanup model.",
    },
];

/// The speech model with this id.
pub fn stt_model_by_id(model_id: &str) -> Option<SttModel> {
    stt_models().into_iter().find(|model| model.id == model_id)
}

/// What the machine offers (`models.HardwareInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct HardwareInfo {
    pub system: String,
    pub machine: String,
    pub ram_gb: f64,
    pub apple_silicon: bool,
    pub free_disk_gb: f64,
}

impl HardwareInfo {
    pub fn new(
        system: &str,
        machine: &str,
        ram_gb: f64,
        apple_silicon: bool,
        free_disk_gb: f64,
    ) -> Self {
        HardwareInfo {
            system: system.to_string(),
            machine: machine.to_string(),
            ram_gb,
            apple_silicon,
            free_disk_gb,
        }
    }
}

/// Failure in the pure setup logic (`ValueError` / `StopIteration` in Python).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupError(pub String);

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

// ------------------------------------------------------------- platform ---

/// macOS microphone authorization (`AVAuthorizationStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicAuthorization {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

/// Everything about the machine the setup logic asks. Implement it in the
/// platform layer; use [`FakeSetupPlatform`] in tests.
pub trait SetupPlatform {
    /// Python `platform.system()`: `"Darwin"`, `"Linux"`, `"Windows"`.
    fn system(&self) -> String;
    /// Python `platform.machine()`: `"arm64"`, `"aarch64"`, `"x86_64"`, ...
    fn machine(&self) -> String;
    /// `shutil.which(command)` is not `None`.
    fn which(&self, command: &str) -> bool;
    /// `models.ollama_installed()`.
    fn ollama_installed(&self) -> bool;
    /// `OllamaClient(base_url).is_running()`; `None` when not probed.
    fn ollama_running(&self, base_url: &str) -> Option<bool>;
    /// `audio.list_input_devices()`; `Err` when the devices cannot be listed.
    fn input_devices(&self) -> std::result::Result<Vec<String>, String>;
    /// `models.runtime_available(backend)`.
    fn runtime_available(&self, backend: &str) -> bool;
    /// `AVCaptureDevice.authorizationStatusForMediaType_`; `None` = unknown.
    fn mic_authorization(&self) -> Option<MicAuthorization>;
    /// `AXIsProcessTrusted()`; `None` = unknown.
    fn accessibility_trusted(&self) -> Option<bool>;
    /// `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == granted`; `None` = unknown.
    fn input_monitoring_granted(&self) -> Option<bool>;
    /// `models.detect_hardware()`.
    fn detect_hardware(&self) -> std::result::Result<HardwareInfo, String>;

    /// `sys.platform == "darwin"`.
    fn is_darwin(&self) -> bool {
        self.system() == "Darwin"
    }

    /// Apple Silicon: Darwin on `arm64`/`aarch64`.
    fn is_apple_silicon(&self) -> bool {
        self.is_darwin() && matches!(self.machine().as_str(), "arm64" | "aarch64")
    }
}

/// `launchagent.status/install/uninstall` (macOS start-on-login).
pub trait LaunchAgent {
    /// `"running"`, `"loaded, not running"`, `"not installed"`, ...
    fn status(&self) -> std::result::Result<String, String>;
    fn install(&self) -> std::result::Result<(), String>;
    fn uninstall(&self) -> std::result::Result<(), String>;
}

/// A launch agent that is not available (non-macOS or not wired): status is an
/// error, so nothing is assumed installed or loaded, and changes fail loudly.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableLaunchAgent;

impl LaunchAgent for UnavailableLaunchAgent {
    fn status(&self) -> std::result::Result<String, String> {
        Err("launch agent management is not available".to_string())
    }
    fn install(&self) -> std::result::Result<(), String> {
        Err("launch agent management is not available".to_string())
    }
    fn uninstall(&self) -> std::result::Result<(), String> {
        Err("launch agent management is not available".to_string())
    }
}

/// The real host, limited to what the standard library can establish:
/// the OS/architecture, executables on `PATH` and the Ollama app bundle.
/// Everything else is reported as unknown/unavailable — never as success:
/// permissions and devices are unknown unless a fuller platform is injected.
/// The shipped Python faster-whisper runtime is configurable; no Rust speech
/// engine is implied.
///
/// [`runtime_available`]: SetupPlatform::runtime_available
#[derive(Clone, Copy, Debug, Default)]
pub struct HostPlatform;

fn which_in(command: &str, path_var: Option<std::ffi::OsString>, exts: &[String]) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let base = dir.join(command);
        if is_executable(&base) {
            return true;
        }
        for ext in exts {
            let mut name = base.clone().into_os_string();
            name.push(ext);
            if is_executable(Path::new(&name)) {
                return true;
            }
        }
    }
    false
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

#[cfg(unix)]
fn detect_host_hardware() -> std::result::Result<HardwareInfo, String> {
    use std::os::unix::ffi::OsStrExt as _;

    // SAFETY: sysconf is a thread-safe read of static system values.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: sysconf is a thread-safe read of static system values.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        return Err("could not detect physical memory".into());
    }
    let ram_gb = pages as f64 * page_size as f64 / 1_000_000_000.0;
    let home = lf_core::home::python_home().map_err(|error| error.to_string())?;
    let path = std::ffi::CString::new(home.as_os_str().as_bytes())
        .map_err(|_| "the home path contains a NUL byte".to_string())?;
    // SAFETY: statvfs writes to a valid, uniquely-owned value and `path` is NUL
    // terminated for the duration of the call.
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: a zero return initialized the statvfs value.
    let stats = unsafe { stats.assume_init() };
    let free_disk_gb = stats.f_bavail as f64 * stats.f_frsize as f64 / 1_000_000_000.0;
    let system = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let machine = std::env::consts::ARCH;
    Ok(HardwareInfo::new(
        system,
        machine,
        ram_gb,
        system == "Darwin" && matches!(machine, "arm64" | "aarch64"),
        free_disk_gb,
    ))
}

#[cfg(not(unix))]
fn detect_host_hardware() -> std::result::Result<HardwareInfo, String> {
    Err("hardware detection is not implemented on this host".into())
}

impl SetupPlatform for HostPlatform {
    fn system(&self) -> String {
        match std::env::consts::OS {
            "macos" => "Darwin",
            "linux" => "Linux",
            "windows" => "Windows",
            other => other,
        }
        .to_string()
    }
    fn machine(&self) -> String {
        std::env::consts::ARCH.to_string()
    }
    fn which(&self, command: &str) -> bool {
        let exts: Vec<String> = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_string)
                .collect()
        } else {
            Vec::new()
        };
        which_in(command, std::env::var_os("PATH"), &exts)
    }
    fn ollama_installed(&self) -> bool {
        self.which("ollama") || Path::new("/Applications/Ollama.app").exists()
    }
    fn ollama_running(&self, _base_url: &str) -> Option<bool> {
        None
    }
    fn input_devices(&self) -> std::result::Result<Vec<String>, String> {
        Err("audio input devices are listed by the platform layer".to_string())
    }
    fn runtime_available(&self, backend: &str) -> bool {
        // The native setup configures the shipped Python runtime; it does not
        // pretend lf-ui itself owns a SpeechEngine. Other runtimes remain
        // unavailable until the platform layer establishes them.
        backend == "faster-whisper"
    }
    fn mic_authorization(&self) -> Option<MicAuthorization> {
        None
    }
    fn accessibility_trusted(&self) -> Option<bool> {
        None
    }
    fn input_monitoring_granted(&self) -> Option<bool> {
        None
    }
    fn detect_hardware(&self) -> std::result::Result<HardwareInfo, String> {
        detect_host_hardware()
    }
}

/// Scriptable in-memory [`SetupPlatform`] for tests and previews.
#[derive(Clone, Debug)]
pub struct FakeSetupPlatform {
    pub system: String,
    pub machine: String,
    /// Commands `which` finds.
    pub commands: Vec<String>,
    pub ollama_installed: bool,
    pub ollama_running: Option<bool>,
    pub input_devices: std::result::Result<Vec<String>, String>,
    /// Backends whose runtime is importable (`faster-whisper` is always
    /// available in Python; list it here to mirror that).
    pub runtimes: Vec<String>,
    pub mic: Option<MicAuthorization>,
    pub accessibility: Option<bool>,
    pub input_monitoring: Option<bool>,
    pub hardware: std::result::Result<HardwareInfo, String>,
    /// Calls that must not happen (Python tests' `AssertionError` probes).
    pub forbid_ollama_probe: bool,
}

impl Default for FakeSetupPlatform {
    fn default() -> Self {
        FakeSetupPlatform {
            system: "Linux".to_string(),
            machine: "x86_64".to_string(),
            commands: Vec::new(),
            ollama_installed: false,
            ollama_running: Some(false),
            input_devices: Ok(Vec::new()),
            runtimes: vec!["faster-whisper".to_string()],
            mic: None,
            accessibility: None,
            input_monitoring: None,
            hardware: Ok(HardwareInfo::new("Linux", "x86_64", 8.0, false, 50.0)),
            forbid_ollama_probe: false,
        }
    }
}

impl SetupPlatform for FakeSetupPlatform {
    fn system(&self) -> String {
        self.system.clone()
    }
    fn machine(&self) -> String {
        self.machine.clone()
    }
    fn which(&self, command: &str) -> bool {
        self.commands.iter().any(|c| c == command)
    }
    fn ollama_installed(&self) -> bool {
        assert!(!self.forbid_ollama_probe, "must not probe Ollama");
        self.ollama_installed
    }
    fn ollama_running(&self, _base_url: &str) -> Option<bool> {
        assert!(!self.forbid_ollama_probe, "must not probe Ollama");
        self.ollama_running
    }
    fn input_devices(&self) -> std::result::Result<Vec<String>, String> {
        self.input_devices.clone()
    }
    fn runtime_available(&self, backend: &str) -> bool {
        self.runtimes.iter().any(|r| r == backend)
    }
    fn mic_authorization(&self) -> Option<MicAuthorization> {
        self.mic
    }
    fn accessibility_trusted(&self) -> Option<bool> {
        self.accessibility
    }
    fn input_monitoring_granted(&self) -> Option<bool> {
        self.input_monitoring
    }
    fn detect_hardware(&self) -> std::result::Result<HardwareInfo, String> {
        self.hardware.clone()
    }
}

/// Recording [`LaunchAgent`] for tests.
#[derive(Debug)]
pub struct FakeLaunchAgent {
    pub status: std::result::Result<String, String>,
    pub calls: Mutex<Vec<&'static str>>,
}

impl FakeLaunchAgent {
    pub fn new(status: &str) -> Self {
        FakeLaunchAgent {
            status: Ok(status.to_string()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn failing(message: &str) -> Self {
        FakeLaunchAgent {
            status: Err(message.to_string()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    fn record(&self, name: &'static str) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(name);
        }
    }
}

impl LaunchAgent for FakeLaunchAgent {
    fn status(&self) -> std::result::Result<String, String> {
        self.record("status");
        self.status.clone()
    }
    fn install(&self) -> std::result::Result<(), String> {
        self.record("install");
        Ok(())
    }
    fn uninstall(&self) -> std::result::Result<(), String> {
        self.record("uninstall");
        Ok(())
    }
}

// ------------------------------------------------------ models functions ---

/// `models.runtime_available`: backends without an optional runtime are
/// always available; the platform decides for `mlx-whisper` and `parakeet`.
/// (`faster-whisper` is also asked of the platform, because `lf-ui` itself
/// links no speech runtime.)
pub fn runtime_available(platform: &dyn SetupPlatform, backend: &str) -> bool {
    platform.runtime_available(backend)
}

/// `models.available_stt_models`.
pub fn available_stt_models(hw: &HardwareInfo, platform: &dyn SetupPlatform) -> Vec<SttModel> {
    stt_models()
        .into_iter()
        .filter(|model| !model.apple_silicon_only || hw.apple_silicon)
        .filter(|model| runtime_available(platform, model.backend))
        .collect()
}

/// `models.recommend`: `(stt id, cleanup model id)`.
pub fn recommend(hw: &HardwareInfo, platform: &dyn SetupPlatform) -> (String, Option<String>) {
    let stt_id = if hw.apple_silicon && runtime_available(platform, "mlx-whisper") {
        if hw.ram_gb >= 16.0 {
            "mlx-small"
        } else {
            "mlx-base"
        }
    } else if hw.ram_gb >= 8.0 {
        "fw-base"
    } else {
        "fw-tiny"
    };
    let llm_id = if hw.ram_gb >= 16.0 {
        Some("llama3.2:3b".to_string())
    } else if hw.ram_gb >= 8.0 {
        Some("llama3.2:1b".to_string())
    } else {
        None
    };
    (stt_id.to_string(), llm_id)
}

/// `models.stt_config`: the `{"stt": {...}}` config update for a model.
pub fn stt_config(model: &SttModel) -> Value {
    let mut stt = Map::new();
    stt.insert("provider".into(), json!(model.backend));
    stt.insert("model".into(), json!(model.model_size));
    if matches!(model.backend, "mlx-whisper" | "parakeet") {
        stt.insert("device".into(), json!("auto"));
    }
    let mut root = Map::new();
    root.insert("stt".into(), Value::Object(stt));
    Value::Object(root)
}

/// `models.current_stt_id`: the catalog id of the configured speech model,
/// `"fw-base"` when the configured one is not in the catalog.
pub fn current_stt_id(config: &Config, platform: &dyn SetupPlatform) -> String {
    let backend = config.field_str("stt_backend").unwrap_or("");
    let size = config.field_str("model_size").unwrap_or("");
    if backend == "parakeet" {
        return "parakeet-v2".to_string();
    }
    let candidate = match backend {
        "mlx" | "mlx-whisper" => format!("mlx-{size}"),
        "faster-whisper" => format!("fw-{size}"),
        "auto" => {
            if platform.is_apple_silicon() && platform.runtime_available("mlx-whisper") {
                format!("mlx-{size}")
            } else {
                format!("fw-{size}")
            }
        }
        _ => format!("fw-{size}"),
    };
    if stt_model_by_id(&candidate).is_some() {
        candidate
    } else {
        "fw-base".to_string()
    }
}

// ------------------------------------------------------------ setup flow ---

/// One checkup row (`ok`: `Some(true)` ready, `Some(false)` needs you,
/// `None` unknown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub key: String,
    pub label: String,
    pub ok: Option<bool>,
    pub detail: String,
    pub fix_label: String,
    pub fix_url: String,
}

impl Check {
    pub fn new(
        key: &str,
        label: &str,
        ok: Option<bool>,
        detail: impl Into<String>,
        fix_label: &str,
        fix_url: &str,
    ) -> Self {
        Check {
            key: key.to_string(),
            label: label.to_string(),
            ok,
            detail: detail.into(),
            fix_label: fix_label.to_string(),
            fix_url: fix_url.to_string(),
        }
    }
}

/// `SetupChoices.paste_last_hotkey`'s default for the platform.
pub fn default_paste_last_hotkey(darwin: bool) -> &'static str {
    if darwin {
        "<ctrl>+<cmd>+v"
    } else {
        "<ctrl>+<shift>+v"
    }
}

/// What the user picked in setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupChoices {
    pub stt_id: String,
    pub llm_id: Option<String>,
    pub input_device: String,
    pub hotkey: String,
    pub language: Option<String>,
    pub start_on_login: bool,
    pub llm_provider: String,
    pub cleanup_level: String,
    pub paste_last_hotkey: String,
    pub command_hotkey: String,
    pub polish_hotkey: String,
    pub prompt_hotkey: String,
    pub pause_media_while_dictating: bool,
    pub max_recording_seconds: i64,
}

impl SetupChoices {
    /// The six positional fields with the Python dataclass defaults for the
    /// rest (`llm_provider="ollama"`, `cleanup_level="light"`, the host's
    /// paste-last hotkey, `alt_r`, `<ctrl>+<alt>+1`, `<ctrl>+<alt>+2`, media
    /// pause off, 1200 seconds).
    pub fn new(
        stt_id: &str,
        llm_id: Option<&str>,
        input_device: &str,
        hotkey: &str,
        language: Option<&str>,
        start_on_login: bool,
    ) -> Self {
        Self::new_for(
            cfg!(target_os = "macos"),
            stt_id,
            llm_id,
            input_device,
            hotkey,
            language,
            start_on_login,
        )
    }

    /// [`new`](Self::new) with the platform default chosen explicitly.
    pub fn new_for(
        darwin: bool,
        stt_id: &str,
        llm_id: Option<&str>,
        input_device: &str,
        hotkey: &str,
        language: Option<&str>,
        start_on_login: bool,
    ) -> Self {
        SetupChoices {
            stt_id: stt_id.to_string(),
            llm_id: llm_id.map(str::to_string),
            input_device: input_device.to_string(),
            hotkey: hotkey.to_string(),
            language: language.map(str::to_string),
            start_on_login,
            llm_provider: "ollama".to_string(),
            cleanup_level: "light".to_string(),
            paste_last_hotkey: default_paste_last_hotkey(darwin).to_string(),
            command_hotkey: "alt_r".to_string(),
            polish_hotkey: "<ctrl>+<alt>+1".to_string(),
            prompt_hotkey: "<ctrl>+<alt>+2".to_string(),
            pause_media_while_dictating: false,
            max_recording_seconds: 1200,
        }
    }
}

/// `(config key, label)` of the hotkey choices.
pub const HOTKEY_OPTIONS: [(&str, &str); 6] = [
    ("alt_l", "Hold Left Option"),
    ("alt_r", "Hold Right Option"),
    ("cmd_r", "Hold Right Command"),
    ("ctrl_r", "Hold Right Control"),
    ("f19", "Hold F19"),
    ("<cmd>+<shift>+<space>", "Press ⌘⇧Space to start/stop"),
];

const SETTINGS_MIC: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
const SETTINGS_ACCESSIBILITY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const SETTINGS_INPUT_MONITORING: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

/// The microphone permission row from an authorization status (`None` = the
/// status could not be read).
pub fn microphone_check(status: Option<MicAuthorization>) -> Check {
    let (ok, detail) = match status {
        None => (None, "Unknown (permission status unavailable)"),
        Some(MicAuthorization::NotDetermined) => (None, "Unknown"),
        Some(MicAuthorization::Authorized) => (Some(true), "Granted"),
        Some(_) => (Some(false), "Not granted"),
    };
    Check::new(
        "microphone",
        "Microphone permission",
        ok,
        detail,
        "Open settings",
        SETTINGS_MIC,
    )
}

fn permission_check(key: &str, label: &str, granted: Option<bool>, fix_url: &str) -> Check {
    let detail = match granted {
        None => "Unknown",
        Some(true) => "Granted",
        Some(false) => "Not granted",
    };
    Check::new(key, label, granted, detail, "Open settings", fix_url)
}

/// `_mac_permission_checks`: microphone, accessibility, input monitoring.
pub fn mac_permission_checks(platform: &dyn SetupPlatform) -> Vec<Check> {
    vec![
        microphone_check(platform.mic_authorization()),
        permission_check(
            "accessibility",
            "Accessibility permission",
            platform.accessibility_trusted(),
            SETTINGS_ACCESSIBILITY,
        ),
        permission_check(
            "input-monitoring",
            "Input Monitoring permission",
            platform.input_monitoring_granted(),
            SETTINGS_INPUT_MONITORING,
        ),
    ]
}

/// `run_checks`: ffmpeg, the cleanup model (Ollama or another provider), the
/// input device and, on macOS, the privacy permissions.
pub fn run_checks(cfg: &Config, platform: &dyn SetupPlatform) -> Vec<Check> {
    let ffmpeg_installed = platform.which("ffmpeg");
    let mut checks = vec![Check::new(
        "ffmpeg",
        "ffmpeg",
        Some(ffmpeg_installed),
        if ffmpeg_installed {
            "Installed"
        } else {
            "Not found — run: brew install ffmpeg"
        },
        "Install ffmpeg",
        "https://formulae.brew.sh/formula/ffmpeg",
    )];
    let provider = cfg.field_str("llm_provider").unwrap_or("");
    if provider == "ollama" {
        let exists = platform.ollama_installed();
        checks.push(Check::new(
            "ollama-installed",
            "Ollama installed",
            Some(exists),
            if exists { "Installed" } else { "Not installed" },
            "Get Ollama",
            "https://ollama.com/download",
        ));
        let running = platform.ollama_running(cfg.field_str("llm_base_url").unwrap_or(""));
        checks.push(Check::new(
            "ollama-running",
            "Ollama running",
            running,
            match running {
                Some(true) => "Ready",
                Some(false) => "Not running",
                None => "Unknown",
            },
            "Start Ollama",
            "https://ollama.com/download",
        ));
    } else {
        checks.push(Check::new(
            "llm-provider",
            "Cleanup model",
            None,
            format!(
                "Using {provider} ({})",
                cfg.field_str("llm_model").unwrap_or("")
            ),
            "Open settings",
            "",
        ));
    }
    checks.push(match platform.input_devices() {
        Ok(devices) => Check::new(
            "microphone-device",
            "Input device",
            Some(!devices.is_empty()),
            if devices.is_empty() {
                "No input devices found".to_string()
            } else {
                format!("{} input device(s) found", devices.len())
            },
            "Check microphone",
            "",
        ),
        // The devices could not be listed at all: unknown, not "none found".
        Err(reason) => Check::new(
            "microphone-device",
            "Input device",
            None,
            format!("Unknown ({reason})"),
            "Check microphone",
            "",
        ),
    });
    if platform.is_darwin() {
        checks.extend(mac_permission_checks(platform));
    }
    checks
}

/// `initial_choices`: what setup pre-selects, from the saved config after a
/// first run, from the hardware recommendation before it.
pub fn initial_choices(
    cfg: &Config,
    hw: &HardwareInfo,
    platform: &dyn SetupPlatform,
    launch_agent: &dyn LaunchAgent,
) -> std::result::Result<SetupChoices, SetupError> {
    let (mut recommended_stt, recommended_llm) = recommend(hw, platform);
    let available = available_stt_models(hw, platform);
    let available_ids: Vec<&str> = available.iter().map(|m| m.id.as_str()).collect();
    if !available_ids.contains(&recommended_stt.as_str()) {
        recommended_stt = available
            .iter()
            .find(|m| m.backend == "faster-whisper")
            .map(|m| m.id.clone())
            .ok_or_else(|| SetupError("no faster-whisper speech model is available".into()))?;
    }
    let text = |key: &str| cfg.field_str(key).unwrap_or("").to_string();
    let provider = text("llm_provider");
    let common = |stt_id: String, llm_id: Option<String>, start_on_login: bool| SetupChoices {
        stt_id,
        llm_id,
        input_device: text("input_device"),
        hotkey: text("hotkey"),
        language: cfg.field_str("language").map(str::to_string),
        start_on_login,
        llm_provider: provider.clone(),
        cleanup_level: text("cleanup_level"),
        paste_last_hotkey: text("paste_last_hotkey"),
        command_hotkey: text("command_hotkey"),
        polish_hotkey: text("polish_hotkey"),
        prompt_hotkey: text("prompt_hotkey"),
        pause_media_while_dictating: cfg
            .field_bool("pause_media_while_dictating")
            .unwrap_or(false),
        max_recording_seconds: cfg.field_i64("max_recording_seconds").unwrap_or(1200),
    };
    if cfg.field_i64("setup_version").unwrap_or(0) > 0 {
        let mut stt_id = current_stt_id(cfg, platform);
        if !available_ids.contains(&stt_id.as_str()) {
            stt_id = recommended_stt;
        }
        let model = text("llm_model");
        let llm_id = (provider == "ollama"
            && cfg.field_bool("cleanup_enabled").unwrap_or(false)
            && LLM_MODELS.iter().any(|m| m.id == model))
        .then_some(model);
        let mut start_on_login = false;
        if hw.system == "Darwin" {
            if let Ok(status) = launch_agent.status() {
                start_on_login = status.starts_with("running") || status.starts_with("loaded");
            }
        }
        return Ok(common(stt_id, llm_id, start_on_login));
    }
    let llm_id = if provider == "ollama" && platform.ollama_installed() {
        recommended_llm
    } else {
        None
    };
    Ok(common(recommended_stt, llm_id, false))
}

/// `stt_config_from_id`: the STT config update for a catalog id.
pub fn stt_config_from_id(model_id: &str) -> std::result::Result<Value, SetupError> {
    match stt_model_by_id(model_id) {
        Some(model) => Ok(stt_config(&model)),
        None => Err(SetupError(format!("unknown speech model: {model_id}"))),
    }
}

/// The sectioned config update `apply` writes for `choices` on top of
/// `current` (pure; no I/O).
pub fn config_update(
    choices: &SetupChoices,
    current: &Config,
) -> std::result::Result<Value, SetupError> {
    let mut updates = stt_config_from_id(&choices.stt_id)?;
    let root = updates
        .as_object_mut()
        .ok_or_else(|| SetupError("stt config is not an object".into()))?;
    if let Some(Value::Object(stt)) = root.get_mut("stt") {
        stt.insert("language".into(), json!(choices.language));
    }
    root.insert(
        "dictation".into(),
        json!({
            "input_device": choices.input_device,
            "pause_media": choices.pause_media_while_dictating,
            "max_recording_seconds": choices.max_recording_seconds,
        }),
    );
    root.insert(
        "hotkey".into(),
        json!({
            "key": choices.hotkey,
            "paste_last": choices.paste_last_hotkey,
            "command": choices.command_hotkey,
            "polish": choices.polish_hotkey,
            "prompt": choices.prompt_hotkey,
        }),
    );
    root.insert("setup".into(), json!({"version": 1}));
    root.insert("cleanup".into(), json!({"level": choices.cleanup_level}));
    if choices.llm_provider == "ollama" {
        let model = choices
            .llm_id
            .clone()
            .unwrap_or_else(|| current.field_str("llm_model").unwrap_or("").to_string());
        root.insert(
            "llm".into(),
            json!({"model": model, "cleanup_enabled": choices.llm_id.is_some()}),
        );
    }
    Ok(updates)
}

/// Where setup reads and writes the configuration.
pub trait SetupConfigStore {
    fn load(&self) -> Result<Config>;
    fn save(&self, config: &Config) -> Result<PathBuf>;
}

/// The config file: the platform default location (`load_config` /
/// `save_config`) or an explicit path. Tests must use [`at`](Self::at) with a
/// temporary path; [`default_location`](Self::default_location) touches the
/// user's real config.
#[derive(Clone, Debug, Default)]
pub struct FileConfigStore {
    path: Option<PathBuf>,
}

impl FileConfigStore {
    /// The user's real config file (and its legacy migration).
    pub fn default_location() -> Self {
        FileConfigStore { path: None }
    }

    /// A specific file; a missing file loads the defaults.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        FileConfigStore {
            path: Some(path.into()),
        }
    }
}

impl SetupConfigStore for FileConfigStore {
    fn load(&self) -> Result<Config> {
        match &self.path {
            None => load_config(),
            Some(path) if path.exists() => load_from(path),
            Some(_) => Ok(Config::default()),
        }
    }

    fn save(&self, config: &Config) -> Result<PathBuf> {
        save_config(config, self.path.as_deref())
    }
}

/// In-memory [`SetupConfigStore`] recording every save (for tests).
#[derive(Debug)]
pub struct MemoryConfigStore {
    pub current: Config,
    pub saved: Mutex<Vec<Config>>,
    pub path: PathBuf,
}

impl MemoryConfigStore {
    pub fn new(current: Config, path: impl Into<PathBuf>) -> Self {
        MemoryConfigStore {
            current,
            saved: Mutex::new(Vec::new()),
            path: path.into(),
        }
    }

    pub fn saved(&self) -> Vec<Config> {
        self.saved.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl SetupConfigStore for MemoryConfigStore {
    fn load(&self) -> Result<Config> {
        Ok(self.current.clone())
    }

    fn save(&self, config: &Config) -> Result<PathBuf> {
        if let Ok(mut saved) = self.saved.lock() {
            saved.push(config.clone());
        }
        Ok(self.path.clone())
    }
}

/// `apply`: write the choices to the config, then install/remove the launch
/// agent on macOS. Returns the config path. The launch agent is only touched
/// after the config was written, like Python.
pub fn apply(
    choices: &SetupChoices,
    store: &dyn SetupConfigStore,
    platform: &dyn SetupPlatform,
    launch_agent: &dyn LaunchAgent,
) -> Result<PathBuf> {
    let path = crate::config_write::with_lock(|| -> Result<PathBuf> {
        let current = store.load()?;
        let updates = config_update(choices, &current)?;
        store.save(&apply_config_update(&current, &updates)?)
    })?;
    if platform.system() == "Darwin" {
        if choices.start_on_login {
            launch_agent.install().map_err(anyhow::Error::msg)?;
        } else if launch_agent.status().map_err(anyhow::Error::msg)? != "not installed" {
            launch_agent.uninstall().map_err(anyhow::Error::msg)?;
        }
    }
    Ok(path)
}

// ------------------------------------------------------------------ audio ---

/// RMS level of one audio block, capped at 1.0 (`MicLevelMonitor`'s callback;
/// an empty block is silence).
pub fn block_level(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / samples.len() as f64).sqrt().min(1.0)
}

/// An open microphone stream.
pub trait MicInputStream {
    fn stop(&mut self);
}

/// Called with each captured block of mono samples (on the audio thread).
pub type BlockCallback = Box<dyn Fn(&[f32]) + Send + Sync>;

/// Opens 16 kHz mono float32 input streams (`sounddevice.InputStream`).
pub trait MicInput {
    fn open(
        &self,
        device: Option<&str>,
        sample_rate: u32,
        on_block: BlockCallback,
    ) -> std::result::Result<Box<dyn MicInputStream>, String>;
}

/// Meters the microphone level for the setup window.
pub struct MicLevelMonitor {
    pub device: Option<String>,
    stream: Option<Box<dyn MicInputStream>>,
    level: Arc<Mutex<f64>>,
}

impl MicLevelMonitor {
    /// An empty device name means the default device.
    pub fn new(device: Option<&str>) -> Self {
        MicLevelMonitor {
            device: device.filter(|d| !d.is_empty()).map(str::to_string),
            stream: None,
            level: Arc::new(Mutex::new(0.0)),
        }
    }

    pub fn start(&mut self, input: &dyn MicInput) -> std::result::Result<(), String> {
        let level = Arc::clone(&self.level);
        let stream = input.open(
            self.device.as_deref(),
            16000,
            Box::new(move |block: &[f32]| {
                if let Ok(mut current) = level.lock() {
                    *current = block_level(block);
                }
            }),
        )?;
        self.stream = Some(stream);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop();
        }
        if let Ok(mut level) = self.level.lock() {
            *level = 0.0;
        }
    }

    pub fn level(&self) -> f64 {
        self.level.lock().map(|l| *l).unwrap_or(0.0)
    }
}

/// `audio.Recorder`.
pub trait AudioRecorder {
    fn start(&mut self) -> std::result::Result<(), String>;
    /// Stop and return the recorded mono samples.
    fn stop(&mut self) -> Vec<f32>;
}

/// Creates recorders (`Recorder(sample_rate=..., device=...)`).
pub trait RecorderFactory {
    fn create(&self, sample_rate: u32, device: Option<&str>) -> Box<dyn AudioRecorder>;
}

/// The speech-to-text engine (`Transcriber.transcribe`).
pub trait Transcriber {
    fn transcribe(&self, audio: &[f32]) -> std::result::Result<String, String>;
}

/// `try_dictation`: record for `seconds` (the injected `sleep`), then
/// transcribe. The recorder is always stopped, even if `sleep` panics.
pub fn try_dictation(
    seconds: f64,
    device: Option<&str>,
    factory: &dyn RecorderFactory,
    transcriber: &dyn Transcriber,
    sleep: &dyn Fn(f64),
) -> std::result::Result<String, String> {
    let mut recorder = factory.create(16000, device.filter(|d| !d.is_empty()));
    recorder.start()?;
    let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sleep(seconds)));
    let audio = recorder.stop();
    if let Err(panic) = waited {
        std::panic::resume_unwind(panic);
    }
    transcriber.transcribe(&audio)
}

#[cfg(test)]
#[path = "setup_flow/tests.rs"]
mod tests;
