//! The contract between the Hub shell ([`super::hub`]) and its pages
//! ([`super::pages`]).
//!
//! In Python every page is a method of the one `HubWindowController`. Here a
//! page is a long-lived object implementing [`HubPage`]: the shell keeps it for
//! the Hub's lifetime, so drafts, search text and the like live in the page and
//! survive the full re-layout the shell performs on every resize, appearance
//! change and navigation. Before the views are torn down the shell calls
//! [`HubPage::capture`] (read live controls into the page state) and afterwards
//! [`HubPage::render`] rebuilds the views from that state.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use lf_core::config::{apply_config_update, load_config, load_from, Config};
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use serde_json::Value;

use super::actions::ActionTarget;
use super::style::FlippedView;
use crate::backends::ask::{AskBackend, FakeAskBackend};
use crate::backends::meeting::{FakeMeetingClient, MeetingClient};
use crate::backends::prompts::{FakePromptQueue, PromptQueue};
use crate::backends::settings_agent::{FakeSettingsAgent, SettingsAgent};
use crate::tokens::{Palette, Weight};

/// The text-measure closure pages pass to the step-1 layout code.
pub type Measure = Rc<dyn Fn(&str, f64, Weight) -> f64>;

/// Everything the Hub reads and writes, injected so snapshots and tests never
/// touch the user's real files.
pub struct HubServices {
    /// The live config snapshot; replaced after every successful save.
    pub config: RefCell<Config>,
    /// Where config updates are written (`None`: the default config path).
    pub config_path: RefCell<Option<PathBuf>>,
    /// When true (snapshots, tests) config updates stay in memory only.
    pub memory_only_config: Cell<bool>,
    pub history: Rc<HistoryStore>,
    pub dictionary: Rc<DictionaryStore>,
    pub prompt_queue: Arc<dyn PromptQueue>,
    pub meeting_client: Arc<dyn MeetingClient>,
    pub ask_backend: RefCell<Box<dyn AskBackend>>,
    pub settings_agent: RefCell<Box<dyn SettingsAgent>>,
    /// Injected by the single native window router. Snapshots leave it unset.
    pub setup_opener: RefCell<Option<Rc<dyn Fn()>>>,
    /// Per-page inline error text (`page_errors` in Python), cleared when the
    /// user navigates to the page from the sidebar.
    pub page_errors: RefCell<HashMap<String, String>>,
    /// Per-page notices (`page_notices`).
    pub page_notices: RefCell<HashMap<String, String>>,
}

impl HubServices {
    pub fn new(
        config: Config,
        history: Rc<HistoryStore>,
        dictionary: Rc<DictionaryStore>,
        prompt_queue: Arc<dyn PromptQueue>,
        meeting_client: Arc<dyn MeetingClient>,
        ask_backend: Box<dyn AskBackend>,
    ) -> Rc<Self> {
        Rc::new(HubServices {
            config: RefCell::new(config),
            config_path: RefCell::new(None),
            memory_only_config: Cell::new(false),
            history,
            dictionary,
            prompt_queue,
            meeting_client,
            ask_backend: RefCell::new(ask_backend),
            settings_agent: RefCell::new(Box::new(FakeSettingsAgent::default())),
            setup_opener: RefCell::new(None),
            page_errors: RefCell::default(),
            page_notices: RefCell::default(),
        })
    }

    /// Like [`Self::new`] but config updates stay in memory (snapshots, tests):
    /// no config file is ever written.
    pub fn new_isolated(
        config: Config,
        history: Rc<HistoryStore>,
        dictionary: Rc<DictionaryStore>,
    ) -> Rc<Self> {
        let services = Self::new(
            config,
            history,
            dictionary,
            Arc::new(FakePromptQueue::new()),
            Arc::new(FakeMeetingClient::new()),
            Box::new(FakeAskBackend::default()),
        );
        services.memory_only_config.set(true);
        services
    }

    /// A copy of the current config.
    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    fn latest_config(&self) -> Result<Config> {
        if self.memory_only_config.get() {
            return Ok(self.config());
        }
        match self.config_path.borrow().as_deref() {
            Some(path) => load_from(path),
            None => load_config(),
        }
    }

    pub fn dictation_config(&self) -> Config {
        let mut config = self.config();
        if let Ok(latest) = self.latest_config() {
            if let Some(enabled) = latest.field_bool("dictation_enabled") {
                config.set_field("dictation_enabled", Value::Bool(enabled));
            }
        }
        config
    }

    /// The saved `dictation_enabled` from this Hub's config source (its
    /// configured path, or the in-memory config for snapshots and tests).
    /// `None` when it cannot be read or is not a bool: no default is assumed.
    pub fn persisted_dictation(&self) -> Option<bool> {
        self.latest_config().ok()?.field_bool("dictation_enabled")
    }

    pub fn update_dictation(&self, enabled: bool) -> Result<Config> {
        self.update_config(&serde_json::json!({"dictation": {"enabled": enabled}}))
    }

    /// Validate and apply a nested config update (`{"section": {"key": v}}`),
    /// write it (unless [`Self::memory_only_config`]) and make it current. The
    /// current config is unchanged on error.
    pub fn update_config(&self, update: &Value) -> Result<Config> {
        let updated = if self.memory_only_config.get() {
            apply_config_update(&self.config(), update)?
        } else {
            crate::config_write::update(self.config_path.borrow().as_deref(), |latest| {
                apply_config_update(latest, update)
            })?
        };
        *self.config.borrow_mut() = updated.clone();
        Ok(updated)
    }

    pub fn set_page_error(&self, page: &str, message: &str) {
        self.page_errors
            .borrow_mut()
            .insert(page.to_string(), message.to_string());
    }

    pub fn page_error(&self, page: &str) -> Option<String> {
        self.page_errors.borrow().get(page).cloned()
    }

    pub fn clear_page_error(&self, page: &str) {
        self.page_errors.borrow_mut().remove(page);
    }
}

/// What a page needs to build its views.
pub struct PageCtx {
    pub mtm: MainThreadMarker,
    /// The page canvas (a flipped document view): add every control here.
    pub body: Retained<FlippedView>,
    /// Content width of the canvas.
    pub width: f64,
    /// Visible height of the scroll viewport (pages may be taller; they scroll).
    pub viewport_height: f64,
    pub palette: Palette,
    pub services: Rc<HubServices>,
    /// Target/action + delegate dispatcher for every control on the page.
    pub target: Retained<ActionTarget>,
    pub measure: Measure,
    /// Ask the shell to navigate (`show_page`).
    pub navigate: Rc<dyn Fn(&str)>,
    /// Ask the shell to rebuild the current page (after a data change).
    pub rebuild: Rc<dyn Fn()>,
}

/// One Hub page.
pub trait HubPage {
    /// The page id (`"Home"`, ...), one of [`crate::hub_route::HUB_PAGES`].
    fn name(&self) -> &'static str;

    /// Build the page into `ctx.body` starting at `top` (below the shared
    /// title and any callout). Returns the y of the bottom of the content; the
    /// shell grows the document to it so tall pages scroll.
    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64;

    /// Read live controls (search text, typed drafts, selections) into the page
    /// state. Called right before the views are torn down.
    fn capture(&mut self) {}

    /// Drop native controls after capture without discarding drafts or work.
    fn release_views(&mut self) {}

    /// Deliver queued request results on the main thread for audits/snapshots.
    /// Return the number of requests still awaiting delivery.
    fn drain_requests(&mut self) -> usize {
        0
    }

    fn pending_requests(&self) -> usize {
        0
    }

    /// Prepare an isolated native snapshot by its explicit plan identifier.
    /// Unknown identifiers fail loudly; snapshots never fall back to a different
    /// state's screenshot. The fixture root contains only temporary data.
    fn prepare_snapshot(&mut self, preparation: &str, _root: &std::path::Path) -> Result<()> {
        anyhow::ensure!(
            preparation.is_empty(),
            "unknown snapshot state {preparation:?}"
        );
        Ok(())
    }

    /// Called when the page is left or the window closes: write anything a
    /// debounced timer was still holding. Must never panic.
    fn flush(&mut self) {}

    /// Called once when the Hub closes: stop timers and drop view references.
    fn shutdown(&mut self) {
        self.flush();
    }

    /// Whether leaving is allowed (an invalid draft may keep the user here).
    fn can_leave(&mut self) -> bool {
        true
    }
}

/// Shared mutable handle to a page (closures hold `Weak` clones of it).
pub type PageHandle = Rc<RefCell<dyn HubPage>>;

/// A convenience flag cell used by pages for "rebuild requested".
pub type Flag = Rc<Cell<bool>>;

#[cfg(test)]
mod tests {
    use super::*;
    use lf_core::config::save_config;
    use std::path::Path;

    fn file_services(dir: &Path, path: &Path, config: Config) -> Rc<HubServices> {
        let services = HubServices::new_isolated(
            config,
            Rc::new(
                HistoryStore::open(
                    Some(dir.join("history.sqlite")),
                    Some(dir.join("clips")),
                    true,
                )
                .unwrap(),
            ),
            Rc::new(DictionaryStore::new(Some(dir.join("dictionary.json")))),
        );
        services.memory_only_config.set(false);
        *services.config_path.borrow_mut() = Some(path.to_path_buf());
        services
    }

    #[test]
    fn config_updates_preserve_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::defaults_for(true);
        config.set_field("dictation_enabled", Value::Bool(true));
        save_config(&config, Some(&path)).unwrap();
        let services = file_services(dir.path(), &path, config);

        let normal = services
            .update_config(&serde_json::json!({"cleanup": {"level": "medium"}}))
            .unwrap();
        assert_eq!(normal.field_str("cleanup_level"), Some("medium"));
        assert_eq!(services.config().field_str("cleanup_level"), Some("medium"));
        assert_eq!(
            load_from(&path).unwrap().field_str("cleanup_level"),
            Some("medium")
        );

        let mut external = load_from(&path).unwrap();
        external.set_field("dictation_enabled", Value::Bool(false));
        save_config(&external, Some(&path)).unwrap();

        let updated = services
            .update_config(&serde_json::json!({"cleanup": {"level": "light"}}))
            .unwrap();
        assert_eq!(updated.field_str("cleanup_level"), Some("light"));
        assert_eq!(updated.field_bool("dictation_enabled"), Some(false));
        assert_eq!(
            services.config().field_bool("dictation_enabled"),
            Some(false)
        );
        let on_disk = load_from(&path).unwrap();
        assert_eq!(on_disk.field_str("cleanup_level"), Some("light"));
        assert_eq!(on_disk.field_bool("dictation_enabled"), Some(false));
    }

    #[test]
    fn config_updates_preserve_current_config_on_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::defaults_for(true);
        config.set_field("dictation_enabled", Value::Bool(true));
        config.set_field("cleanup_level", Value::String("medium".into()));
        save_config(&config, Some(&path)).unwrap();
        let services = file_services(dir.path(), &path, config);
        std::fs::write(&path, "not valid TOML [").unwrap();

        assert!(services
            .update_config(&serde_json::json!({"cleanup": {"level": "light"}}))
            .is_err());
        assert_eq!(services.config().field_str("cleanup_level"), Some("medium"));
        assert_eq!(
            services.config().field_bool("dictation_enabled"),
            Some(true)
        );
    }

    #[test]
    fn config_updates_preserve_current_config_on_save_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::defaults_for(true);
        config.set_field("dictation_enabled", Value::Bool(true));
        config.set_field("cleanup_level", Value::String("medium".into()));
        save_config(&config, Some(&path)).unwrap();
        let services = file_services(dir.path(), &path, config);

        let original_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut read_only = original_permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(dir.path(), read_only).unwrap();
        let result = services.update_config(&serde_json::json!({"cleanup": {"level": "light"}}));
        std::fs::set_permissions(dir.path(), original_permissions).unwrap();

        assert!(result.is_err());
        assert_eq!(services.config().field_str("cleanup_level"), Some("medium"));
        assert_eq!(
            services.config().field_bool("dictation_enabled"),
            Some(true)
        );
        let on_disk = load_from(&path).unwrap();
        assert_eq!(on_disk.field_str("cleanup_level"), Some("medium"));
        assert_eq!(on_disk.field_bool("dictation_enabled"), Some(true));
    }

    #[test]
    fn dictation_updates_reload_disk_and_preserve_config_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config::defaults_for(true);
        save_config(&config, Some(&path)).unwrap();
        let services = HubServices::new_isolated(
            config.clone(),
            Rc::new(
                HistoryStore::open(
                    Some(dir.path().join("history.sqlite")),
                    Some(dir.path().join("clips")),
                    true,
                )
                .unwrap(),
            ),
            Rc::new(DictionaryStore::new(Some(
                dir.path().join("dictionary.json"),
            ))),
        );
        services.memory_only_config.set(false);
        *services.config_path.borrow_mut() = Some(path.clone());
        let mut external = config;
        external.set_field("dictation_enabled", Value::Bool(false));
        external.set_field("cleanup_level", Value::String("light".into()));
        save_config(&external, Some(&path)).unwrap();
        assert_eq!(
            services.dictation_config().field_bool("dictation_enabled"),
            Some(false)
        );
        let updated = services.update_dictation(true).unwrap();
        assert_eq!(updated.field_bool("dictation_enabled"), Some(true));
        assert_eq!(
            load_from(&path).unwrap().field_bool("dictation_enabled"),
            Some(true)
        );
        assert_eq!(updated.field_str("cleanup_level"), Some("light"));
        std::fs::write(&path, "not valid TOML [").unwrap();
        assert!(services.update_dictation(false).is_err());
        assert_eq!(
            services.config().field_bool("dictation_enabled"),
            Some(true)
        );
        services.memory_only_config.set(true);
        services.update_dictation(false).unwrap();
        assert_eq!(
            services.config().field_bool("dictation_enabled"),
            Some(false)
        );
    }
}
