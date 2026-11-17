//! Python-aligned snapshot fixtures for the ported Hub pages.

pub const SNAPSHOT_PAGES: [&str; 5] = crate::hub_route::HUB_PAGES;
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
pub enum DictionarySeed {
    None,
    /// `LocalFlow` and `teh -> the`.
    Hub,
    /// Eight words and four replacements.
    Responsive,
}

/// Prompt queue contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueSeed {
    Empty,
    /// One pending prompt and one failed with "Orca is not running".
    Two,
    /// Three prompts: distinct from the Python `two` fixture.
    Three,
    /// Two failed prompts with different causes.
    Failures,
}

/// Interactions run on the Style page before the shot (real handlers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleStep {
    None,
    /// Explicitly disable cleanup: a distinct state, not an alias of default.
    Empty,
    /// Click cleanup "Medium" and Work "Casual" with writes succeeding.
    Autosaved,
    /// Richer saved selections for the required populated state.
    Populated,
    /// After [`StyleStep::Autosaved`], click Email "Excited" while the config
    /// cannot be written.
    SaveError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scenario {
    pub page: &'static str,
    pub state: &'static str,
    pub history: HistorySeed,
    pub dictionary: DictionarySeed,
    pub queue: QueueSeed,
    pub style: StyleStep,
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

pub const SCENARIOS: [Scenario; 19] = [
    Scenario {
        page: "Home",
        state: "empty",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Home",
        state: "populated",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Home",
        state: "dictation-off",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: false,
        error: None,
    },
    Scenario {
        page: "History",
        state: "empty",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "History",
        state: "populated",
        history: HistorySeed::Hub,
        dictionary: DictionarySeed::Hub,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "History",
        state: "error",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: Some("History is unavailable."),
    },
    Scenario {
        page: "Dictionary",
        state: "empty",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Dictionary",
        state: "populated",
        history: HistorySeed::Hub,
        dictionary: DictionarySeed::Hub,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Dictionary",
        state: "entries",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Dictionary",
        state: "error",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: Some("Unable to load local data."),
    },
    Scenario {
        page: "Style",
        state: "default",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Style",
        state: "empty",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::Empty,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Style",
        state: "populated",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::Populated,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Style",
        state: "autosaved",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::Autosaved,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Style",
        state: "save-error",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::SaveError,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Prompts",
        state: "empty",
        history: HistorySeed::None,
        dictionary: DictionarySeed::None,
        queue: QueueSeed::Empty,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Prompts",
        state: "two",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Two,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Prompts",
        state: "failures",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Failures,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
    },
    Scenario {
        page: "Prompts",
        state: "populated",
        history: HistorySeed::Responsive,
        dictionary: DictionarySeed::Responsive,
        queue: QueueSeed::Three,
        style: StyleStep::None,
        dictation_enabled: true,
        error: None,
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

/// The vocabulary words of a seed.
pub fn dictionary_words(seed: DictionarySeed) -> Vec<&'static str> {
    match seed {
        DictionarySeed::None => Vec::new(),
        DictionarySeed::Hub => vec!["LocalFlow"],
        DictionarySeed::Responsive => vec![
            "LocalFlow",
            "Kubernetes",
            "Fireworks",
            "Orca",
            "PyObjC",
            "Obsidian",
            "Whisper",
            "Ollama",
        ],
    }
}

/// The `(from, to)` replacements of a seed.
pub fn dictionary_replacements(seed: DictionarySeed) -> Vec<(&'static str, &'static str)> {
    match seed {
        DictionarySeed::None => Vec::new(),
        DictionarySeed::Hub => vec![("teh", "the")],
        DictionarySeed::Responsive => vec![
            ("teh", "the"),
            ("my email", "me@example.com"),
            ("lf", "LocalFlow"),
            ("k8s", "Kubernetes"),
        ],
    }
}

/// One queued prompt of a [`QueueSeed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedPrompt {
    pub title: &'static str,
    pub body: &'static str,
    /// `None`: stays pending; `Some(error)`: marked failed with this error.
    pub failed_with: Option<&'static str>,
}

/// The prompts of a queue seed, in queue order.
pub fn queue_prompts(seed: QueueSeed) -> Vec<SeedPrompt> {
    const REDIS: &str = "Migrate session storage to Redis";
    const DOCS: &str = "Update deploy docs for Redis";
    const FILES: &str = "Replace local files.";
    const DEPENDENCY: &str = "Document the new dependency.";
    match seed {
        QueueSeed::Empty => Vec::new(),
        QueueSeed::Two => vec![
            SeedPrompt {
                title: REDIS,
                body: FILES,
                failed_with: None,
            },
            SeedPrompt {
                title: DOCS,
                body: DEPENDENCY,
                failed_with: Some("Orca is not running"),
            },
        ],
        QueueSeed::Three => vec![
            SeedPrompt {
                title: REDIS,
                body: FILES,
                failed_with: None,
            },
            SeedPrompt {
                title: DOCS,
                body: DEPENDENCY,
                failed_with: None,
            },
            SeedPrompt {
                title: "Verify the Redis rollback runbook",
                body: "Capture the rollback owner and test results.",
                failed_with: None,
            },
        ],
        QueueSeed::Failures => vec![
            SeedPrompt {
                title: REDIS,
                body: FILES,
                failed_with: Some("Orca app is not running — run `orca open`"),
            },
            SeedPrompt {
                title: DOCS,
                body: DEPENDENCY,
                failed_with: Some("orca exited 2: repo not found"),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn dictionary_style_and_prompt_fixtures_match_python() {
        assert_eq!(
            find("Home", "populated").unwrap().dictionary,
            DictionarySeed::Responsive
        );
        assert_eq!(dictionary_words(DictionarySeed::Responsive).len(), 8);
        assert_eq!(dictionary_replacements(DictionarySeed::Responsive).len(), 4);
        assert_eq!(dictionary_words(DictionarySeed::Hub), ["LocalFlow"]);
        assert_eq!(queue_prompts(QueueSeed::Two).len(), 2);
        assert_eq!(queue_prompts(QueueSeed::Three).len(), 3);
        let failures = queue_prompts(QueueSeed::Failures);
        assert_eq!(failures.len(), 2);
        assert!(failures.iter().all(|entry| entry.failed_with.is_some()));
        assert!(queue_prompts(QueueSeed::Empty).is_empty());
        assert!(dictionary_words(DictionarySeed::None).is_empty());
        assert_eq!(
            find("Style", "autosaved").unwrap().style,
            StyleStep::Autosaved
        );
        assert_eq!(
            find("Style", "save-error").unwrap().style,
            StyleStep::SaveError
        );
        assert_eq!(
            find("Dictionary", "entries").unwrap().dictionary,
            DictionarySeed::Responsive
        );
        assert_eq!(find("Prompts", "two").unwrap().queue, QueueSeed::Two);
    }

    #[test]
    fn only_ported_pages_have_empty_and_populated_snapshots() {
        assert_eq!(SNAPSHOT_PAGES, crate::hub_route::HUB_PAGES);
        assert_eq!(total_snapshots(), 76);
        for page in SNAPSHOT_PAGES {
            assert!(history_rows(find(page, "empty").unwrap().history).is_empty());
            let populated = find(page, "populated").unwrap();
            assert!(
                populated.history != HistorySeed::None
                    || populated.dictionary != DictionarySeed::None
                    || populated.queue != QueueSeed::Empty
                    || populated.style != StyleStep::None
            );
            assert!(scenarios_for(page).len() >= 3);
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
        let visible: Vec<_> = rows
            .iter()
            .rev()
            .take(crate::hub_model::HISTORY_PAGE_SIZE as usize)
            .collect();
        assert_eq!(visible[0].text, "History item 10.");
        assert_eq!(visible.last().unwrap().text, "History item 3.");
        assert!(visible.iter().all(|row| !row.clip));
        assert_eq!(history_rows(HistorySeed::Responsive).len(), 4);
    }
}
