//! The manual settings: every row maps to an existing `[section] key` of the
//! LocalFlow config. Values are validated by `apply_config_update` (and the
//! shortcut rules from `lf_core::shortcuts`) before anything is written.

use anyhow::{anyhow, Result};
use lf_core::config::{self, Config};
use serde_json::{json, Value};

use crate::data::ConfigFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Shortcuts,
    Dictation,
    Cleanup,
    Meetings,
    Privacy,
    Advanced,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::General,
        Tab::Shortcuts,
        Tab::Dictation,
        Tab::Cleanup,
        Tab::Meetings,
        Tab::Privacy,
        Tab::Advanced,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Shortcuts => "Shortcuts",
            Tab::Dictation => "Input & language",
            Tab::Cleanup => "Cleanup",
            Tab::Meetings => "Meetings",
            Tab::Privacy => "Privacy & data",
            Tab::Advanced => "Advanced",
        }
    }
}

/// A choice value as stored in TOML.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lit {
    Str(&'static str),
    Int(i64),
    Null,
}

impl Lit {
    pub fn to_value(self) -> Value {
        match self {
            Lit::Str(s) => json!(s),
            Lit::Int(i) => json!(i),
            Lit::Null => Value::Null,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    Switch,
    Choice(&'static [(Lit, &'static str)]),
    /// Microphone: choices come from `lf_audio::list_input_devices`.
    Microphone,
    Shortcut {
        optional: bool,
    },
    Text {
        placeholder: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub tab: Tab,
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub control: Control,
    /// Shown disabled with this reason on this platform.
    pub unavailable: Option<&'static str>,
    /// Takes effect after LocalFlow restarts.
    pub restart: bool,
}

const LANGUAGES: &[(Lit, &str)] = &[
    (Lit::Null, "Detect automatically"),
    (Lit::Str("en"), "English"),
    (Lit::Str("es"), "Spanish"),
    (Lit::Str("fr"), "French"),
    (Lit::Str("de"), "German"),
    (Lit::Str("it"), "Italian"),
    (Lit::Str("pt"), "Portuguese"),
    (Lit::Str("nl"), "Dutch"),
    (Lit::Str("pl"), "Polish"),
    (Lit::Str("ja"), "Japanese"),
    (Lit::Str("zh"), "Chinese"),
    (Lit::Str("ko"), "Korean"),
    (Lit::Str("ar"), "Arabic"),
    (Lit::Str("hi"), "Hindi"),
];

const PASTE: &[(Lit, &str)] = &[
    (Lit::Str("auto"), "Automatic"),
    (Lit::Str("clipboard"), "Clipboard only"),
    (Lit::Str("wtype"), "wtype (Wayland)"),
    (Lit::Str("xdotool"), "xdotool (X11)"),
    (Lit::Str("pynput"), "pynput"),
    (Lit::Str("osascript"), "osascript (macOS)"),
];

const MAX_RECORDING: &[(Lit, &str)] = &[
    (Lit::Int(300), "5 minutes"),
    (Lit::Int(600), "10 minutes"),
    (Lit::Int(1200), "20 minutes"),
    (Lit::Int(0), "No limit"),
];

const CLIPS: &[(Lit, &str)] = &[
    (Lit::Int(0), "Don't keep audio"),
    (Lit::Int(5), "Last 5"),
    (Lit::Int(20), "Last 20"),
    (Lit::Int(50), "Last 50"),
];

pub const CLEANUP_LEVELS: &[(Lit, &str)] = &[
    (Lit::Str("none"), "None"),
    (Lit::Str("light"), "Light"),
    (Lit::Str("medium"), "Medium"),
];

const LINUX_NO_SOUNDS: Option<&str> = if cfg!(target_os = "macos") {
    None
} else {
    Some("Sounds aren't available on this platform yet.")
};
const NO_MEETING_DETECT: Option<&str> = if cfg!(target_os = "macos") {
    None
} else {
    Some("Meeting detection isn't available on this platform yet.")
};

const fn row(
    tab: Tab,
    section: &'static str,
    key: &'static str,
    label: &'static str,
    description: &'static str,
    control: Control,
) -> Spec {
    Spec {
        tab,
        section,
        key,
        label,
        description,
        control,
        unavailable: None,
        restart: false,
    }
}

pub const SPECS: &[Spec] = &[
    row(
        Tab::General,
        "dictation",
        "enabled",
        "Dictation",
        "Push-to-talk is active while LocalFlow runs.",
        Control::Switch,
    ),
    Spec {
        unavailable: LINUX_NO_SOUNDS,
        ..row(
            Tab::General,
            "hotkey",
            "sounds",
            "Sounds",
            "Play a short cue when recording starts and stops.",
            Control::Switch,
        )
    },
    row(
        Tab::Shortcuts,
        "hotkey",
        "key",
        "Push-to-talk",
        "Hold to dictate, release to paste.",
        Control::Shortcut { optional: false },
    ),
    row(
        Tab::Shortcuts,
        "hotkey",
        "dictation_toggle",
        "Toggle dictation shortcut",
        "Turn push-to-talk on or off without quitting LocalFlow.",
        Control::Shortcut { optional: true },
    ),
    row(
        Tab::Shortcuts,
        "hotkey",
        "command",
        "Command Mode",
        "Hold and speak an instruction to rewrite your selection.",
        Control::Shortcut { optional: true },
    ),
    row(
        Tab::Shortcuts,
        "hotkey",
        "polish",
        "Polish",
        "Clean up the selected text in place.",
        Control::Shortcut { optional: true },
    ),
    row(
        Tab::Shortcuts,
        "hotkey",
        "prompt",
        "Prompt Engineer",
        "Turn rough selected text into an AI-ready prompt.",
        Control::Shortcut { optional: true },
    ),
    row(
        Tab::Shortcuts,
        "hotkey",
        "paste_last",
        "Paste last",
        "Paste your most recent dictation again.",
        Control::Shortcut { optional: true },
    ),
    row(
        Tab::Dictation,
        "dictation",
        "input_device",
        "Microphone",
        "The input LocalFlow records from.",
        Control::Microphone,
    ),
    row(
        Tab::Dictation,
        "stt",
        "language",
        "Language",
        "What you speak. Support depends on the speech model.",
        Control::Choice(LANGUAGES),
    ),
    row(
        Tab::Dictation,
        "paste",
        "method",
        "Paste method",
        "How text reaches the focused app.",
        Control::Choice(PASTE),
    ),
    row(
        Tab::Dictation,
        "dictation",
        "spoken_punctuation",
        "Spoken punctuation",
        "Say “comma”, “period” or “new line”.",
        Control::Switch,
    ),
    row(
        Tab::Dictation,
        "dictation",
        "spoken_symbols",
        "Spoken symbols",
        "Say “at sign” or “hashtag” to type symbols.",
        Control::Switch,
    ),
    row(
        Tab::Dictation,
        "features",
        "hot_phrases",
        "Snippets",
        "Expand saved text when you say a trigger.",
        Control::Switch,
    ),
    row(
        Tab::Dictation,
        "dictation",
        "pause_media",
        "Pause media while dictating",
        "Pause playing audio while you speak.",
        Control::Switch,
    ),
    row(
        Tab::Dictation,
        "dictation",
        "max_recording_seconds",
        "Longest recording",
        "A cue plays a minute before the limit; the clip is still transcribed.",
        Control::Choice(MAX_RECORDING),
    ),
    row(
        Tab::Cleanup,
        "llm",
        "cleanup_enabled",
        "Cleanup",
        "Tidy grammar and filler words with your cleanup model.",
        Control::Switch,
    ),
    row(
        Tab::Cleanup,
        "cleanup",
        "level",
        "Cleanup level",
        "How much LocalFlow may edit what you said.",
        Control::Choice(CLEANUP_LEVELS),
    ),
    row(
        Tab::Cleanup,
        "cleanup",
        "styles_enabled",
        "Writing styles",
        "Adjust tone by app category (set tones on the Style page).",
        Control::Switch,
    ),
    Spec {
        restart: true,
        ..row(
            Tab::Meetings,
            "features",
            "meetings",
            "Meetings",
            "Record meetings and save notes to your vault.",
            Control::Switch,
        )
    },
    Spec {
        unavailable: NO_MEETING_DETECT,
        ..row(
            Tab::Meetings,
            "meetings",
            "watch",
            "Meeting detection",
            "Notice when a call starts.",
            Control::Switch,
        )
    },
    Spec {
        unavailable: NO_MEETING_DETECT,
        ..row(
            Tab::Meetings,
            "meetings",
            "auto_start",
            "Auto-start meetings",
            "Start recording when a meeting is detected.",
            Control::Switch,
        )
    },
    row(
        Tab::Meetings,
        "meetings",
        "capture_system_audio",
        "Capture system audio",
        "Record other participants too. Applies from the next meeting.",
        Control::Switch,
    ),
    row(
        Tab::Meetings,
        "meetings",
        "system_audio_device",
        "System audio device",
        "On Linux, a monitor input such as “Monitor of Built-in Audio”.",
        Control::Text {
            placeholder: "Monitor input name",
        },
    ),
    row(
        Tab::Meetings,
        "meetings",
        "default_template",
        "Default template",
        "Template id for new meetings.",
        Control::Text {
            placeholder: "general",
        },
    ),
    row(
        Tab::Meetings,
        "meetings",
        "work_prompts",
        "Work prompts",
        "Draft follow-up prompts from meetings for review.",
        Control::Switch,
    ),
    row(
        Tab::Meetings,
        "meetings",
        "vault_path",
        "Notes vault",
        "Folder where meeting notes are saved.",
        Control::Text {
            placeholder: "~/notes",
        },
    ),
    row(
        Tab::Meetings,
        "meetings",
        "notes_folder",
        "Notes folder",
        "Folder inside the vault.",
        Control::Text {
            placeholder: "notes/meetings",
        },
    ),
    row(
        Tab::Privacy,
        "history",
        "enabled",
        "Save history",
        "Keep dictations on this device so you can search and copy them.",
        Control::Switch,
    ),
    row(
        Tab::Privacy,
        "history",
        "keep_audio_clips",
        "Keep audio clips",
        "Recent recordings kept for troubleshooting.",
        Control::Choice(CLIPS),
    ),
];

pub fn specs_for(tab: Tab) -> impl Iterator<Item = &'static Spec> {
    SPECS.iter().filter(move |s| s.tab == tab)
}

pub fn spec(section: &str, key: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.section == section && s.key == key)
}

impl Spec {
    pub fn field(&self) -> &'static str {
        config::config_field(&format!("{}.{}", self.section, self.key))
            .map(|(_, field)| field)
            .expect("spec names a real config key")
    }

    pub fn current(&self, config: &Config) -> Value {
        config.field(self.field()).cloned().unwrap_or(Value::Null)
    }

    /// Label of the current choice; an unlisted stored value is shown as is.
    pub fn choice_label(&self, config: &Config) -> String {
        let current = self.current(config);
        if let Control::Choice(choices) = self.control {
            if let Some((_, label)) = choices.iter().find(|(lit, _)| lit.to_value() == current) {
                return (*label).to_string();
            }
        }
        match current {
            Value::String(s) if s.is_empty() => "Not set".into(),
            Value::String(s) => s,
            Value::Null => "Not set".into(),
            other => other.to_string(),
        }
    }
}

/// The platform name the shortcut rules expect.
pub fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        "linux"
    }
}

/// Validate and save one setting. Returns the saved config.
pub fn save(file: &ConfigFile, spec: &Spec, value: Value) -> Result<Config> {
    if let Some(reason) = spec.unavailable {
        return Err(anyhow!(reason));
    }
    let value = match spec.control {
        Control::Shortcut { optional } => {
            let text = value.as_str().unwrap_or("").trim().to_string();
            if text.is_empty() && !optional {
                return Err(anyhow!("Push-to-talk needs a key."));
            }
            let normalized = lf_core::shortcuts::normalize_optional_combo(&text, platform())
                .map_err(|e| anyhow!("That shortcut isn't valid: {e}"))?;
            json!(normalized)
        }
        Control::Text { .. } => json!(value.as_str().unwrap_or("").trim()),
        _ => value,
    };
    let update = json!({ spec.section: { spec.key: value } });
    file.update(|latest| {
        let updated = config::apply_config_update(latest, &update)?;
        if let Control::Shortcut { .. } = spec.control {
            let field = spec.field();
            if let Some((_, other)) =
                lf_core::shortcuts::shortcut_conflicts(&updated, &[field], platform()).first()
            {
                let name = SPECS
                    .iter()
                    .find(|s| s.field() == *other)
                    .map_or(*other, |s| s.label);
                return Err(anyhow!("That shortcut is already used by {name}."));
            }
        }
        Ok(updated)
    })
}

/// A short, user-facing version of a save error.
pub fn error_text(error: &anyhow::Error) -> String {
    let text = format!("{error}");
    if let Some(ce) = error.downcast_ref::<config::ConfigError>() {
        return format!("Not saved: {}", ce.0);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::DataRoot;
    use lf_core::config::PASTE_METHODS;

    fn file() -> (tempfile::TempDir, ConfigFile) {
        let dir = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(DataRoot::sandbox(dir.path()));
        (dir, file)
    }

    #[test]
    fn every_spec_names_a_real_key_and_valid_choices() {
        let defaults = Config::defaults_for(false);
        for spec in SPECS {
            let field = spec.field();
            assert!(defaults.field(field).is_some(), "{field}");
            if let Control::Choice(choices) = spec.control {
                for (lit, _) in choices {
                    let update = json!({ spec.section: { spec.key: lit.to_value() } });
                    if spec.section == "paste" {
                        continue;
                    }
                    config::apply_config_update(&defaults, &update)
                        .unwrap_or_else(|e| panic!("{}.{} {lit:?}: {e}", spec.section, spec.key));
                }
            }
        }
        for method in PASTE_METHODS {
            assert!(PASTE.iter().any(|(lit, _)| *lit == Lit::Str(method)));
        }
    }

    #[test]
    fn switches_and_choices_save_durably() {
        let (_d, file) = file();
        let s = spec("history", "enabled").unwrap();
        save(&file, s, json!(false)).unwrap();
        let s = spec("dictation", "max_recording_seconds").unwrap();
        save(&file, s, json!(600)).unwrap();
        let s = spec("stt", "language").unwrap();
        save(&file, s, json!("de")).unwrap();
        let loaded = config::load_from(file.path()).unwrap();
        assert_eq!(loaded.field_bool("history_enabled"), Some(false));
        assert_eq!(loaded.field_i64("max_recording_seconds"), Some(600));
        assert_eq!(loaded.field_str("language"), Some("de"));
        save(&file, s, Value::Null).unwrap();
        assert!(config::load_from(file.path())
            .unwrap()
            .field("language")
            .unwrap()
            .is_null());
    }

    #[test]
    fn invalid_values_are_refused() {
        let (_d, file) = file();
        let s = spec("cleanup", "level").unwrap();
        assert!(save(&file, s, json!("extreme")).is_err());
        let s = spec("dictation", "enabled").unwrap();
        assert!(save(&file, s, json!("on")).is_err());
        let s = spec("paste", "method").unwrap();
        assert!(save(&file, s, json!("telepathy")).is_err());
    }

    #[test]
    fn shortcuts_are_normalized_and_conflicts_named() {
        let (_d, file) = file();
        let polish = spec("hotkey", "polish").unwrap();
        let saved = save(&file, polish, json!("<ctrl>+<alt>+9")).unwrap();
        assert_eq!(saved.field_str("polish_hotkey"), Some("<ctrl>+<alt>+9"));
        let prompt = spec("hotkey", "prompt").unwrap();
        let err = save(&file, prompt, json!("<ctrl>+<alt>+9")).unwrap_err();
        assert!(error_text(&err).contains("Polish"), "{err}");
        let key = spec("hotkey", "key").unwrap();
        assert!(save(&file, key, json!("")).is_err());
        // Optional shortcuts can be turned off.
        let saved = save(&file, polish, json!("")).unwrap();
        assert_eq!(saved.field_str("polish_hotkey"), Some(""));
    }

    #[test]
    fn dictation_toggle_shortcut_name_is_unified() {
        let toggle = spec("hotkey", "dictation_toggle").unwrap();
        assert_eq!(toggle.label, "Toggle dictation shortcut");
        assert_eq!(toggle.label, lf_agent::label("dictation_toggle_hotkey"));

        let (_d, file) = file();
        save(&file, toggle, json!("<ctrl>+<alt>+8")).unwrap();
        let polish = spec("hotkey", "polish").unwrap();
        let err = save(&file, polish, json!("<ctrl>+<alt>+8")).unwrap_err();
        assert!(
            error_text(&err).contains("Toggle dictation shortcut"),
            "{err}"
        );
    }

    #[test]
    fn unavailable_rows_never_write() {
        let (_d, file) = file();
        if let Some(s) = SPECS.iter().find(|s| s.unavailable.is_some()) {
            assert!(save(&file, s, json!(false)).is_err());
            assert!(!file.path().exists());
        }
    }
}
