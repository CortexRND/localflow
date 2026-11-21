//! New-user setup guidance on Home: honest checks with one action each, over
//! the saved configuration and runtime signals. The 7-step wizard itself is
//! `controllers::setup_flow` (Run setup / Resume setup).

use lf_core::config::Config;
use lf_ui::models_layout;

use crate::data::DataRoot;
use crate::page::Page;
use crate::remote::Health;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Done,
    Todo,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub title: &'static str,
    pub detail: String,
    pub state: State,
    /// Where the action goes, with its label.
    pub action: Option<(&'static str, Page, Option<&'static str>)>,
}

/// Whether `[setup] version` or an explicit speech choice is in the file
/// (`hub_route::configuration_marker`, same rule as Python).
pub fn configured(root: &DataRoot) -> bool {
    lf_core::config::parse_toml_file(&root.config_path)
        .map(|data| lf_ui::hub_route::configuration_marker(&data))
        .unwrap_or(false)
}

/// `listening`: a runtime confirmed its shortcut listener (protocol-2 status
/// with a fresh heartbeat); nothing else marks the shortcut step done.
pub fn steps(
    root: &DataRoot,
    config: &Config,
    health: &Health,
    dictations: i64,
    listening: bool,
) -> Vec<Step> {
    let mut out = Vec::new();
    out.push(Step {
        title: "Speech model",
        detail: format!(
            "{} — download or change it in Models & keys.",
            models_layout::speech_summary(config)
        ),
        state: if configured(root) {
            State::Done
        } else {
            State::Todo
        },
        action: Some(("Choose model", Page::Models, Some("speech"))),
    });
    let cleanup_on = config.field_bool("cleanup_enabled").unwrap_or(true);
    out.push(Step {
        title: "Cleanup model",
        detail: if cleanup_on {
            let (_, cleanup) = models_layout::summary_values(config);
            format!("{cleanup}. Optional: tidies grammar and filler words.")
        } else {
            "Cleanup is off; transcripts are pasted as spoken.".into()
        },
        state: State::Done,
        action: Some(("Review", Page::Models, Some("cleanup"))),
    });
    let mic = config.field_str("input_device").unwrap_or("");
    out.push(Step {
        title: "Microphone",
        detail: if mic.is_empty() {
            "Using the system default input.".into()
        } else {
            format!("Using “{mic}”.")
        },
        state: State::Done,
        action: Some(("Change", Page::Settings, Some("input"))),
    });
    let key = lf_ui::hub_model::format_hotkey(config.field_str("hotkey"));
    out.push(Step {
        title: "Shortcut",
        detail: if listening {
            format!("Set to {key}. A running LocalFlow is listening for it.")
        } else {
            format!(
                "Set to {key}. Not confirmed: no running LocalFlow reports its shortcut listener yet."
            )
        },
        state: if listening {
            State::Done
        } else {
            State::Unavailable
        },
        action: Some(("Shortcuts", Page::Settings, Some("shortcuts"))),
    });
    out.push(Step {
        title: "LocalFlow service",
        detail: match health {
            Health::Online => "Reachable: Ask, Meetings and Prompts can connect. This doesn't confirm dictation input.".into(),
            Health::Simulated => "Sandbox: answered by example services in this window.".into(),
            Health::Unknown => "Checking…".into(),
            Health::Offline => "Not reachable. Start LocalFlow with `localflow` (or `lf serve`) for Ask, Meetings and Prompts.".into(),
        },
        state: if health.usable() { State::Done } else { State::Todo },
        action: None,
    });
    out.push(Step {
        title: "First dictation",
        detail: if dictations > 0 {
            format!("{dictations} saved so far.")
        } else {
            "Dictations you make with LocalFlow appear in History.".into()
        },
        state: if dictations > 0 {
            State::Done
        } else {
            State::Todo
        },
        action: Some(("Open History", Page::History, None)),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_reflects_real_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        let config = Config::defaults_for(false);
        let s = steps(&root, &config, &Health::Offline, 0, false);
        assert_eq!(s[0].state, State::Todo, "no config file: not configured");
        let shortcut = |s: &[Step]| s.iter().find(|s| s.title == "Shortcut").unwrap().state;
        assert_eq!(
            shortcut(&s),
            State::Unavailable,
            "never done without a listener"
        );
        assert_eq!(
            shortcut(&steps(&root, &config, &Health::Online, 0, false)),
            State::Unavailable,
            "HTTP health is not a listener"
        );
        assert_eq!(
            shortcut(&steps(&root, &config, &Health::Offline, 0, true)),
            State::Done
        );
        assert_eq!(
            s.iter()
                .find(|s| s.title == "LocalFlow service")
                .unwrap()
                .state,
            State::Todo
        );
        lf_core::config::save_config(&config, Some(&root.config_path)).unwrap();
        let s = steps(&root, &config, &Health::Online, 3, false);
        assert_eq!(s[0].state, State::Done);
        assert!(s
            .iter()
            .all(|s| s.state != State::Todo || s.title == "Speech model"));
    }
}
