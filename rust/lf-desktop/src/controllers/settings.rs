//! The manual settings: every row maps to an existing `[section] key` of the
//! LocalFlow config. Values are validated by `apply_config_update` (and the
//! shortcut rules from `lf_core::shortcuts`) before anything is written.

use anyhow::{anyhow, Result};
use lf_core::config::{self, Config};
use lf_ui::hub_settings::{SpeakerReadiness, SPEAKER_SWITCHES};
use serde_json::{json, Value};

use crate::data::ConfigFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Shortcuts,
    Dictation,
    Cleanup,
    Meetings,
    /// Settings → Agents: service-backed rows (`pages/settings_agents.rs`),
    /// no `SPECS` entries. Shown only while the service speaks agents v1.
    Agents,
    Privacy,
    Advanced,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::General,
        Tab::Shortcuts,
        Tab::Dictation,
        Tab::Cleanup,
        Tab::Meetings,
        Tab::Agents,
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
            Tab::Agents => crate::controllers::agents_settings::TAB_LABEL,
            Tab::Privacy => "Privacy & data",
            Tab::Advanced => "Advanced",
        }
    }

    /// The tab strip: every tab, Agents only while the agents service is
    /// supported (it answers v1, on or off: the tab is where it's turned on).
    pub fn visible(agents_supported: bool) -> Vec<Tab> {
        Tab::ALL
            .into_iter()
            .filter(|t| *t != Tab::Agents || agents_supported)
            .collect()
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
    // Speaker labels: last, so every earlier row keeps its index (and the
    // Meetings tab gains them at the end of its group). Not settings-
    // assistant keys. Both servers read them when a meeting starts; a Python
    // server keeps `diarize_mic` from its own startup, which the copy says.
    row(
        Tab::Meetings,
        "meetings",
        "speaker_diarization",
        SPEAKER_SWITCHES[0].1,
        "Label who is speaking in saved meeting transcripts. Starts with your next meeting.",
        Control::Switch,
    ),
    row(
        Tab::Meetings,
        "meetings",
        "diarize_mic",
        SPEAKER_SWITCHES[1].1,
        "Also tell apart people sharing your microphone. Starts with your next meeting; a Python LocalFlow server needs a restart first.",
        Control::Switch,
    ),
];

/// Whether `spec` is one of the speaker-label switches (gated by readiness).
pub fn is_speaker_switch(spec: &Spec) -> bool {
    spec.section == "meetings" && SPEAKER_SWITCHES.iter().any(|(key, _)| *key == spec.key)
}

/// The speaker switch `spec` as drawn for `config` and `readiness`
/// (`None` for any other row): on can always be turned off; turning one on
/// needs ready models, and the microphone switch needs Speaker labels on.
pub fn speaker_switch(
    spec: &Spec,
    config: &Config,
    readiness: &SpeakerReadiness,
) -> Option<lf_ui::hub_settings::SpeakerSwitch> {
    if !is_speaker_switch(spec) {
        return None;
    }
    let on = |field: &str| config.field_bool(field).unwrap_or(false);
    lf_ui::hub_settings::speaker_switches(
        on("speaker_diarization"),
        on("diarize_mic"),
        readiness,
        false,
    )
    .into_iter()
    .find(|switch| switch.key == spec.key)
}

/// The muted line under a speaker switch. Speaker labels: why the models
/// aren't ready (still checking, no runtime, not downloaded, or the check
/// failed), also while it is on, since labels can't be made then. The
/// microphone row adds only what that row needs (Speaker labels first).
pub fn speaker_switch_note(
    spec: &Spec,
    config: &Config,
    readiness: &SpeakerReadiness,
) -> Option<String> {
    let switch = speaker_switch(spec, config, readiness)?;
    let labels = config.field_bool("speaker_diarization").unwrap_or(false);
    match (spec.key, readiness) {
        ("speaker_diarization", SpeakerReadiness::Ready) => None,
        ("speaker_diarization", _) => lf_ui::hub_settings::speaker_note(readiness, labels),
        (_, SpeakerReadiness::Ready) if !switch.on && !switch.enabled => {
            lf_ui::hub_settings::speaker_note(readiness, labels)
        }
        _ => None,
    }
}

/// Refuse turning a speaker switch on when it isn't available; turning one
/// off is always allowed (never blocked by missing models or a down server).
pub fn check_speaker_switch(
    spec: &Spec,
    value: &Value,
    config: &Config,
    readiness: &SpeakerReadiness,
) -> Result<()> {
    let Some(switch) = speaker_switch(spec, config, readiness) else {
        return Ok(());
    };
    if value.as_bool() == Some(true) && !switch.on && !switch.enabled {
        let labels = config.field_bool("speaker_diarization").unwrap_or(false);
        let reason = lf_ui::hub_settings::speaker_note(readiness, labels)
            .unwrap_or_else(|| lf_ui::hub_settings::SPEAKERS_CHECKING.to_string());
        return Err(anyhow!("Not turned on: {reason}"));
    }
    Ok(())
}

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

    #[test]
    fn the_agents_tab_follows_meetings_and_only_while_supported() {
        let shown = Tab::visible(true);
        assert_eq!(shown, Tab::ALL.to_vec());
        let meetings = shown.iter().position(|t| *t == Tab::Meetings).unwrap();
        assert_eq!(shown[meetings + 1], Tab::Agents);
        assert_eq!(Tab::Agents.label(), "Agents");
        let hidden = Tab::visible(false);
        assert_eq!(hidden.len(), 7);
        assert!(!hidden.contains(&Tab::Agents));
        assert_eq!(
            specs_for(Tab::Agents).count(),
            0,
            "service-backed rows only"
        );
    }

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

    /// The two speaker switches: Meetings tab only, after every older row
    /// (indices unchanged), gated by readiness for on and never for off.
    #[test]
    fn speaker_switches_follow_readiness_and_always_allow_off() {
        use lf_ui::hub_settings::{speaker_readiness, SPEAKERS_MIC_NEEDS_LABELS};
        let labels = spec("meetings", "speaker_diarization").unwrap();
        let mic = spec("meetings", "diarize_mic").unwrap();
        assert_eq!((labels.tab, mic.tab), (Tab::Meetings, Tab::Meetings));
        assert!(!labels.restart && !mic.restart, "read per meeting");
        assert_eq!(&SPECS[SPECS.len() - 2..], &[*labels, *mic]);
        assert!(specs_for(Tab::General).all(|s| !is_speaker_switch(s)));
        assert!(mic.description.contains("restart"));

        let ready = speaker_readiness(Some(&Ok(
            json!({"extra_installed": true, "models_ready": true}),
        )));
        let no_runtime = speaker_readiness(Some(&Ok(
            json!({"extra_installed": false, "models_ready": false}),
        )));
        let missing = speaker_readiness(Some(&Ok(
            json!({"extra_installed": true, "models_ready": false, "reason": ""}),
        )));
        let failed = speaker_readiness(Some(&Err("down".into())));
        let checking = speaker_readiness(None);
        let mut config = Config::defaults_for(false);
        for blocked in [&no_runtime, &missing, &failed, &checking] {
            assert!(check_speaker_switch(labels, &json!(true), &config, blocked).is_err());
            assert!(speaker_switch_note(labels, &config, blocked).is_some());
        }
        assert!(check_speaker_switch(labels, &json!(true), &config, &ready).is_ok());
        assert_eq!(speaker_switch_note(labels, &config, &ready), None);
        // Mic needs labels first.
        let err = check_speaker_switch(mic, &json!(true), &config, &ready).unwrap_err();
        assert!(err.to_string().contains(SPEAKERS_MIC_NEEDS_LABELS), "{err}");
        assert_eq!(
            speaker_switch_note(mic, &config, &ready).as_deref(),
            Some(SPEAKERS_MIC_NEEDS_LABELS)
        );
        config.set_field("speaker_diarization", json!(true));
        assert!(check_speaker_switch(mic, &json!(true), &config, &ready).is_ok());
        // On -> off is always allowed, ready or not; other rows are untouched.
        config.set_field("diarize_mic", json!(true));
        for any in [&ready, &no_runtime, &failed, &checking] {
            assert!(check_speaker_switch(labels, &json!(false), &config, any).is_ok());
            assert!(check_speaker_switch(mic, &json!(false), &config, any).is_ok());
        }
        let history = spec("history", "enabled").unwrap();
        assert!(check_speaker_switch(history, &json!(true), &config, &failed).is_ok());

        // Saving off writes the file even with the server down.
        let (_d, file) = file();
        save(&file, labels, json!(true)).unwrap();
        save(&file, labels, json!(false)).unwrap();
        let loaded = config::load_from(file.path()).unwrap();
        assert_eq!(loaded.field_bool("speaker_diarization"), Some(false));
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
