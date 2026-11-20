//! What the app may honestly say about dictation.
//!
//! Three facts are kept apart:
//! - the **config** (`[dictation] enabled`, `[hotkey] key`): read from the file;
//! - the **local service** (`GET /healthz`): reachable or not. That endpoint
//!   can be served by the API alone, so it says nothing about microphone
//!   capture or global shortcuts;
//! - **input capability** (is a runtime listening for the shortcut?): this
//!   window cannot observe it, so it is always reported as unverified.

use crate::remote::Health;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationStatus {
    /// Card heading (config fact).
    pub lead: String,
    /// Card supporting line.
    pub support: String,
    /// Local service line and its level.
    pub service: (Level, String),
    /// Input/runtime capability line (never claims readiness).
    pub input: String,
    /// Sidebar row: `(level, line 1, line 2)`.
    pub sidebar: (Level, String, String),
}

/// Platform note about global shortcuts, without promising they work.
pub fn shortcut_note(macos: bool) -> &'static str {
    if macos {
        "Global shortcuts are handled by the LocalFlow app (with Accessibility permission), not by this window."
    } else {
        "Global shortcuts are handled by the LocalFlow runtime, not by this window; on Linux they depend on the Python runtime and your desktop session (X11)."
    }
}

pub fn describe(
    sandbox: bool,
    enabled: bool,
    key: &str,
    health: &Health,
    address: &str,
    macos: bool,
) -> DictationStatus {
    let service = match health {
        Health::Simulated => (
            Level::Warn,
            "Sandbox: services are simulated in this window".to_string(),
        ),
        Health::Online => (
            Level::Ok,
            format!("Local service reachable at {address} (Ask, Meetings, Prompts)"),
        ),
        Health::Offline => (
            Level::Warn,
            format!("Local service not reachable at {address}"),
        ),
        Health::Unknown => (Level::Neutral, "Checking the local service…".to_string()),
    };
    if sandbox {
        return DictationStatus {
            lead: "Exploring example data".into(),
            support: "Dictation isn't connected in the sandbox. Everything you change here stays in the sandbox folder.".into(),
            service,
            input: "No microphone or shortcut is used in the sandbox.".into(),
            sidebar: (Level::Warn, "Sandbox".into(), "Example data, fake services".into()),
        };
    }
    let (lead, support) = if enabled {
        (
            "Dictation is enabled in your config".to_string(),
            format!("Push-to-talk shortcut: {key}. {}", shortcut_note(macos)),
        )
    } else {
        (
            "Dictation is turned off in your config".to_string(),
            "The LocalFlow runtime won't start push-to-talk until you turn it on.".to_string(),
        )
    };
    let input = "This window can't check whether a runtime is listening to your microphone or shortcut. Try a dictation in another app to confirm.".to_string();
    let sidebar_line2 = match health {
        Health::Online => "Service reachable",
        Health::Offline => "Service not reachable",
        Health::Unknown => "Checking service…",
        Health::Simulated => "Simulated service",
    };
    let level = match health {
        Health::Offline => Level::Warn,
        _ => Level::Neutral,
    };
    DictationStatus {
        lead,
        support,
        service,
        input,
        sidebar: (
            level,
            if enabled {
                "Dictation enabled"
            } else {
                "Dictation off"
            }
            .into(),
            sidebar_line2.into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERCLAIMS: [&str; 6] = [
        "Hold ",
        "in any app",
        "Ready to dictate",
        "Listening",
        "Dictation on",
        "LocalFlow is running",
    ];

    fn all_text(s: &DictationStatus) -> String {
        format!(
            "{} {} {} {} {} {}",
            s.lead, s.support, s.service.1, s.input, s.sidebar.1, s.sidebar.2
        )
    }

    #[test]
    fn http_health_never_implies_dictation_readiness() {
        for macos in [false, true] {
            for health in [Health::Online, Health::Offline, Health::Unknown] {
                for enabled in [true, false] {
                    let s = describe(
                        false,
                        enabled,
                        "Left Option",
                        &health,
                        "http://127.0.0.1:8756",
                        macos,
                    );
                    let text = all_text(&s);
                    for claim in OVERCLAIMS {
                        assert!(!text.contains(claim), "{claim:?} in {text:?}");
                    }
                    assert!(s.input.contains("can't check"));
                    if health != Health::Offline {
                        assert_ne!(
                            s.sidebar.0,
                            Level::Ok,
                            "the sidebar never shows a green dictation state"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn service_and_config_are_reported_separately() {
        let on = describe(
            false,
            true,
            "Left Option",
            &Health::Online,
            "http://h:1",
            false,
        );
        assert_eq!(on.service.0, Level::Ok);
        assert!(on.service.1.contains("http://h:1"));
        assert!(on.lead.contains("enabled in your config"));
        assert!(on.support.contains("Left Option") && on.support.contains("Linux"));
        let off = describe(
            false,
            false,
            "Left Option",
            &Health::Offline,
            "http://h:1",
            true,
        );
        assert_eq!(off.service.0, Level::Warn);
        assert!(off.lead.contains("off"));
        assert_eq!(off.sidebar.1, "Dictation off");
        let sb = describe(true, true, "x", &Health::Simulated, "", false);
        assert_eq!(sb.sidebar.1, "Sandbox");
    }
}
