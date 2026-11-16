//! Which Hub snapshots exist and what data each one is seeded with (the pure,
//! platform-independent half of `examples/snapshots.rs`, after
//! `scripts/mac_ui_snapshots.py`).
//!
//! A scenario carries its *data* explicitly ([`HistorySeed`],
//! [`DictionarySeed`], [`QueueSeed`], [`StyleStep`]); the renderer seeds from
//! those fields and never from the state's name, and the directory/file ids
//! used for fixtures ([`Scenario::fixture_id`]) are separate from the logical
//! state ([`Scenario::state`]). A populated scenario therefore cannot silently
//! render empty because an id happened to differ from a state word: the tests
//! below pin that.
//!
//! State names are the Python script's where one exists (`hub-prompts-empty`,
//! `-two`, `-failures`; `hub-dictionary-entries`; `hub-style-default`,
//! `-autosaved`, `-save-error`; `hub-home-populated`). The required
//! `populated` / `empty` coverage of every page is kept; where Python has no
//! state of that name the scenario is an explicit alias of the closest Python
//! state (see [`Scenario::alias_of`]).

/// The pages the Rust Hub implements natively.
pub const SNAPSHOT_PAGES: [&str; 5] = ["Home", "History", "Dictionary", "Style", "Prompts"];

/// `(label, width, height)` of the two window sizes (`HUB_SIZES`).
pub const SNAPSHOT_SIZES: [(&str, f64, f64); 2] = [
    (
        "min",
        crate::tokens::HUB_MIN_WIDTH as f64,
        crate::tokens::HUB_MIN_HEIGHT as f64,
    ),
    ("wide", 1280.0, 820.0),
];

/// `(label, dark)` of the two appearances.
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

/// Dictionary entries to seed.
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
    /// Two failed prompts with different causes.
    Failures,
}

/// Interactions run on the Style page before the shot (real handlers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleStep {
    None,
    /// Click cleanup "Medium" and Work "Casual" with writes succeeding.
    Autosaved,
    /// After [`StyleStep::Autosaved`], click Email "Excited" while the config
    /// cannot be written.
    SaveError,
}

/// One snapshot scenario (a page in a state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scenario {
    pub page: &'static str,
    /// The logical state used in the file name.
    pub state: &'static str,
    pub history: HistorySeed,
    pub dictionary: DictionarySeed,
    pub queue: QueueSeed,
    pub style: StyleStep,
    /// The Python state this one repeats under another required name.
    pub alias_of: Option<&'static str>,
}

/// `Prompts` -> `prompts`, `Hot Phrases` -> `hot-phrases`.
pub fn slug(page: &str) -> String {
    page.to_lowercase().replace(' ', "-")
}

impl Scenario {
    /// A unique id for this scenario's fixture files (directories, database and
    /// JSON names). It is derived from page and state but is **never** compared
    /// with a state name: seeding reads the seed fields only.
    pub fn fixture_id(&self) -> String {
        format!("{}-{}", slug(self.page), self.state)
    }

    /// `hub-<page>-<state>-<size>-<theme>.png`.
    pub fn file_name(&self, size: &str, theme: &str) -> String {
        format!("hub-{}-{}-{size}-{theme}.png", slug(self.page), self.state)
    }

    /// Whether any data is seeded (the page must not render its empty state).
    pub fn has_data(&self) -> bool {
        self.history != HistorySeed::None
            || self.dictionary != DictionarySeed::None
            || self.queue != QueueSeed::Empty
            || self.style != StyleStep::None
    }
}

const fn scenario(
    page: &'static str,
    state: &'static str,
    history: HistorySeed,
    dictionary: DictionarySeed,
    queue: QueueSeed,
    style: StyleStep,
    alias_of: Option<&'static str>,
) -> Scenario {
    Scenario {
        page,
        state,
        history,
        dictionary,
        queue,
        style,
        alias_of,
    }
}

use DictionarySeed as D;
use HistorySeed as H;
use QueueSeed as Q;
use StyleStep as S;

/// Every scenario, grouped by page in [`SNAPSHOT_PAGES`] order.
pub const SCENARIOS: [Scenario; 16] = [
    // Home: `hub-home-populated` (responsive data) and the empty first run.
    scenario("Home", "empty", H::None, D::None, Q::Empty, S::None, None),
    scenario(
        "Home",
        "populated",
        H::Responsive,
        D::Responsive,
        Q::Empty,
        S::None,
        None,
    ),
    // History: the `_hub_snapshots` empty / populated data.
    scenario(
        "History",
        "empty",
        H::None,
        D::None,
        Q::Empty,
        S::None,
        None,
    ),
    scenario(
        "History",
        "populated",
        H::Hub,
        D::Hub,
        Q::Empty,
        S::None,
        None,
    ),
    // Dictionary: `hub-dictionary-entries` plus the empty / populated pair.
    scenario(
        "Dictionary",
        "empty",
        H::None,
        D::None,
        Q::Empty,
        S::None,
        None,
    ),
    scenario(
        "Dictionary",
        "populated",
        H::Hub,
        D::Hub,
        Q::Empty,
        S::None,
        None,
    ),
    scenario(
        "Dictionary",
        "entries",
        H::Responsive,
        D::Responsive,
        Q::Empty,
        S::None,
        None,
    ),
    // Style: default / autosaved / save-error; `empty` and `populated` are the
    // required names for the default and the saved selections.
    scenario(
        "Style",
        "default",
        H::None,
        D::None,
        Q::Empty,
        S::None,
        None,
    ),
    scenario(
        "Style",
        "autosaved",
        H::None,
        D::None,
        Q::Empty,
        S::Autosaved,
        None,
    ),
    scenario(
        "Style",
        "save-error",
        H::None,
        D::None,
        Q::Empty,
        S::SaveError,
        None,
    ),
    scenario(
        "Style",
        "empty",
        H::None,
        D::None,
        Q::Empty,
        S::None,
        Some("default"),
    ),
    scenario(
        "Style",
        "populated",
        H::None,
        D::None,
        Q::Empty,
        S::Autosaved,
        Some("autosaved"),
    ),
    // Prompts: `hub-prompts-empty/two/failures`; `populated` repeats `two`.
    scenario(
        "Prompts",
        "empty",
        H::None,
        D::None,
        Q::Empty,
        S::None,
        None,
    ),
    scenario(
        "Prompts",
        "two",
        H::Responsive,
        D::Responsive,
        Q::Two,
        S::None,
        None,
    ),
    scenario(
        "Prompts",
        "failures",
        H::Responsive,
        D::Responsive,
        Q::Failures,
        S::None,
        None,
    ),
    scenario(
        "Prompts",
        "populated",
        H::Responsive,
        D::Responsive,
        Q::Two,
        S::None,
        Some("two"),
    ),
];

/// Number of PNG files a full run writes.
pub fn total_snapshots() -> usize {
    SCENARIOS.len() * SNAPSHOT_SIZES.len() * SNAPSHOT_THEMES.len()
}

/// The scenarios of `page`, in table order.
pub fn scenarios_for(page: &str) -> Vec<&'static Scenario> {
    SCENARIOS.iter().filter(|s| s.page == page).collect()
}

/// The scenario `(page, state)`, if any.
pub fn find(page: &str, state: &str) -> Option<&'static Scenario> {
    SCENARIOS
        .iter()
        .find(|s| s.page == page && s.state == state)
}

/// One seeded history row.
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
    fn a_full_run_is_sixteen_scenarios_times_four_renders() {
        assert_eq!(SCENARIOS.len(), 16);
        assert_eq!(total_snapshots(), 64);
        // The required coverage alone is 5 pages x populated/empty x 2 sizes x 2 themes.
        assert_eq!(
            SNAPSHOT_PAGES.len() * 2 * SNAPSHOT_SIZES.len() * SNAPSHOT_THEMES.len(),
            40
        );
    }

    #[test]
    fn every_page_keeps_populated_and_empty_coverage() {
        for page in SNAPSHOT_PAGES {
            for state in ["populated", "empty"] {
                assert!(find(page, state).is_some(), "{page} {state}");
            }
        }
        assert!(SCENARIOS.iter().all(|s| SNAPSHOT_PAGES.contains(&s.page)));
    }

    #[test]
    fn python_state_names_are_used() {
        for (page, states) in [
            ("Prompts", vec!["empty", "two", "failures"]),
            ("Dictionary", vec!["entries"]),
            ("Style", vec!["default", "autosaved", "save-error"]),
            ("Home", vec!["populated"]),
        ] {
            for state in states {
                assert!(find(page, state).is_some(), "{page} {state}");
            }
        }
    }

    /// The regression this module exists for: a scenario's seeding is decided
    /// by its seed fields, not by comparing a fixture id with a state word, so
    /// every non-empty-state scenario has data and every empty one has none.
    #[test]
    fn populated_scenarios_are_seeded_and_empty_ones_are_not() {
        for scenario in &SCENARIOS {
            let populated_name = !matches!(scenario.state, "empty" | "default");
            if scenario.page == "Style" {
                // Style has no stores: its data is the clicked selections.
                assert_eq!(
                    scenario.style != StyleStep::None,
                    !matches!(scenario.state, "empty" | "default"),
                    "{} {}",
                    scenario.page,
                    scenario.state
                );
                continue;
            }
            assert_eq!(
                scenario.has_data(),
                populated_name,
                "{} {} must {}have data",
                scenario.page,
                scenario.state,
                if populated_name { "" } else { "not " }
            );
        }
        // And the seeds really contain rows.
        assert_eq!(history_rows(HistorySeed::Hub).len(), 12);
        assert!(history_rows(HistorySeed::Hub)[0].clip);
        assert_eq!(history_rows(HistorySeed::Responsive).len(), 4);
        assert_eq!(dictionary_words(DictionarySeed::Responsive).len(), 8);
        assert_eq!(dictionary_replacements(DictionarySeed::Responsive).len(), 4);
        assert_eq!(dictionary_words(DictionarySeed::Hub), vec!["LocalFlow"]);
        assert_eq!(queue_prompts(QueueSeed::Two).len(), 2);
        assert_eq!(queue_prompts(QueueSeed::Failures).len(), 2);
        assert!(history_rows(HistorySeed::None).is_empty());
        assert!(queue_prompts(QueueSeed::Empty).is_empty());
    }

    #[test]
    fn the_populated_pages_select_the_data_they_show() {
        let populated = |page| find(page, "populated").unwrap();
        assert_eq!(populated("History").history, HistorySeed::Hub);
        assert_eq!(populated("Dictionary").dictionary, DictionarySeed::Hub);
        assert_eq!(populated("Home").history, HistorySeed::Responsive);
        assert_eq!(populated("Prompts").queue, QueueSeed::Two);
        assert_eq!(
            find("Dictionary", "entries").unwrap().dictionary,
            DictionarySeed::Responsive
        );
        assert_eq!(
            find("Prompts", "failures").unwrap().queue,
            QueueSeed::Failures
        );
        assert_eq!(
            find("Style", "save-error").unwrap().style,
            StyleStep::SaveError
        );
    }

    #[test]
    fn fixture_ids_and_file_names_are_unique_and_independent_of_the_state_word() {
        let ids: HashSet<String> = SCENARIOS.iter().map(Scenario::fixture_id).collect();
        assert_eq!(ids.len(), SCENARIOS.len());
        let mut files = HashSet::new();
        for scenario in &SCENARIOS {
            for (size, _, _) in SNAPSHOT_SIZES {
                for (theme, _) in SNAPSHOT_THEMES {
                    assert!(files.insert(scenario.file_name(size, theme)));
                }
            }
        }
        assert_eq!(files.len(), total_snapshots());
        let prompts = find("Prompts", "failures").unwrap();
        assert_eq!(prompts.fixture_id(), "prompts-failures");
        assert_eq!(
            prompts.file_name("min", "dark"),
            "hub-prompts-failures-min-dark.png"
        );
        // A fixture id such as "home-populated" is not the state "populated":
        // comparing them (the original bug) would call this scenario empty.
        let home = find("Home", "populated").unwrap();
        assert_ne!(home.fixture_id(), home.state);
        assert!(home.has_data());
    }

    #[test]
    fn aliases_point_at_an_existing_state_of_the_same_page() {
        for scenario in SCENARIOS.iter().filter(|s| s.alias_of.is_some()) {
            let target = find(scenario.page, scenario.alias_of.unwrap()).unwrap();
            assert_eq!(
                (
                    scenario.history,
                    scenario.dictionary,
                    scenario.queue,
                    scenario.style
                ),
                (
                    target.history,
                    target.dictionary,
                    target.queue,
                    target.style
                ),
                "{} {} must seed what {} seeds",
                scenario.page,
                scenario.state,
                target.state
            );
        }
    }

    #[test]
    fn slug_matches_the_python_file_names() {
        assert_eq!(slug("Prompts"), "prompts");
        assert_eq!(slug("Hot Phrases"), "hot-phrases");
    }
}
