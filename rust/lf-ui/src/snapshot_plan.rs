//! Snapshot fixtures for the shell, Home and History only.

pub const SNAPSHOT_PAGES: [&str; 2] = crate::hub_route::HUB_PAGES;
pub const SNAPSHOT_SIZES: [(&str, f64, f64); 2] = [
    (
        "min",
        crate::tokens::HUB_MIN_WIDTH as f64,
        crate::tokens::HUB_MIN_HEIGHT as f64,
    ),
    ("wide", 1280.0, 820.0),
];
pub const SNAPSHOT_THEMES: [(&str, bool); 2] = [("light", false), ("dark", true)];

/// History rows to seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistorySeed {
    None,
    /// `_hub_snapshots` "populated": a clip-backed dictation, a command and ten
    /// dictations (12 rows).
    Hub,
    /// `_hub_responsive_snapshots`: four dictations.
    Responsive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scenario {
    pub page: &'static str,
    pub state: &'static str,
    pub history: HistorySeed,
    pub dictation_enabled: bool,
    pub error: Option<&'static str>,
}

pub fn slug(page: &str) -> String {
    page.to_lowercase().replace(' ', "-")
}

impl Scenario {
    pub fn fixture_id(&self) -> String {
        format!("{}-{}", slug(self.page), self.state)
    }

    pub fn file_name(&self, size: &str, theme: &str) -> String {
        format!("hub-{}-{size}-{theme}.png", self.fixture_id())
    }
}

pub const SCENARIOS: [Scenario; 6] = [
    Scenario {
        page: "Home",
        state: "empty",
        history: HistorySeed::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Home",
        state: "populated",
        history: HistorySeed::Responsive,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Home",
        state: "dictation-off",
        history: HistorySeed::Responsive,
        dictation_enabled: false,
        error: None,
    },
    Scenario {
        page: "History",
        state: "empty",
        history: HistorySeed::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "History",
        state: "populated",
        history: HistorySeed::Hub,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "History",
        state: "error",
        history: HistorySeed::None,
        dictation_enabled: true,
        error: Some("History is unavailable."),
    },
];

pub fn total_snapshots() -> usize {
    SCENARIOS.len() * SNAPSHOT_SIZES.len() * SNAPSHOT_THEMES.len()
}

pub fn scenarios_for(page: &str) -> Vec<&'static Scenario> {
    SCENARIOS
        .iter()
        .filter(|scenario| scenario.page == page)
        .collect()
}

pub fn find(page: &str, state: &str) -> Option<&'static Scenario> {
    SCENARIOS
        .iter()
        .find(|scenario| scenario.page == page && scenario.state == state)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SeedEntry {
    pub kind: &'static str,
    pub app: &'static str,
    pub raw: String,
    pub text: String,
    pub duration_s: f64,
    /// The row has a recorded clip (a 0.1 s silent WAV).
    pub clip: bool,
}

/// The history rows of a seed (`history.add(...)` calls of the Python script).
pub fn history_rows(seed: HistorySeed) -> Vec<SeedEntry> {
    match seed {
        HistorySeed::None => Vec::new(),
        HistorySeed::Hub => {
            let mut rows = vec![
                SeedEntry {
                    kind: "dictate",
                    app: "Editor",
                    raw: "A raw transcript".into(),
                    text: "A polished transcript.".into(),
                    duration_s: 30.0,
                    clip: true,
                },
                SeedEntry {
                    kind: "command",
                    app: "Browser",
                    raw: "Make this concise".into(),
                    text: "A concise selection.".into(),
                    duration_s: 0.0,
                    clip: false,
                },
            ];
            for index in 1..=10 {
                rows.push(SeedEntry {
                    kind: "dictate",
                    app: "Editor",
                    raw: format!("Raw history item {index}"),
                    text: format!("History item {index}."),
                    duration_s: 12.0,
                    clip: false,
                });
            }
            rows
        }
        HistorySeed::Responsive => (0..4)
            .map(|index| SeedEntry {
                kind: "dictate",
                app: "Editor",
                raw: format!("Raw item {index}"),
                text: format!("Dictation number {index}."),
                duration_s: 20.0,
                clip: false,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn only_ported_pages_have_empty_and_populated_snapshots() {
        assert_eq!(SNAPSHOT_PAGES, ["Home", "History"]);
        assert_eq!(total_snapshots(), 24);
        for page in SNAPSHOT_PAGES {
            assert!(history_rows(find(page, "empty").unwrap().history).is_empty());
            assert!(!history_rows(find(page, "populated").unwrap().history).is_empty());
            assert_eq!(scenarios_for(page).len(), 3);
        }
        assert!(find("Meetings", "populated").is_none());
        assert!(find("Home", "missing").is_none());
        assert!(!find("Home", "dictation-off").unwrap().dictation_enabled);
        assert!(find("History", "error").unwrap().error.is_some());
    }

    #[test]
    fn fixtures_and_file_names_are_unique() {
        let mut names = HashSet::new();
        let mut ids = HashSet::new();
        for scenario in SCENARIOS {
            assert!(ids.insert(scenario.fixture_id()));
            for (size, _, _) in SNAPSHOT_SIZES {
                for (theme, _) in SNAPSHOT_THEMES {
                    assert!(names.insert(scenario.file_name(size, theme)));
                }
            }
        }
        assert_eq!(names.len(), total_snapshots());
        assert!(names.contains("hub-home-populated-wide-dark.png"));
    }

    #[test]
    fn history_fixtures_match_python() {
        let rows = history_rows(HistorySeed::Hub);
        assert_eq!(rows.len(), 12);
        assert!(rows[0].clip);
        assert_eq!(rows[1].kind, "command");
        assert_eq!(history_rows(HistorySeed::Responsive).len(), 4);
    }
}
