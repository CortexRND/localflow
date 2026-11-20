//! Page routing for the single LocalFlow Hub instance (port of
//! `localflow/hub_route.py`; AppKit-free).
//!
//! LaunchServices does not deliver new `open --args` arguments to an app that
//! is already running, so a caller that wants the running Hub on a specific
//! page leaves a small request file and the app consumes it from a run-loop
//! timer:
//!
//! ```text
//! caller  -> write_request("Hot Phrases")      (atomic: tmp + rename)
//! app     -> consume_request() every ~0.25 s   (atomic claim, then delete)
//! ```
//!
//! The page is checked against a whitelist on both sides, the file is never
//! evaluated (plain JSON, size capped), it is removed when consumed and a
//! request older than [`STALE_SECONDS`] is dropped, so a leftover file can
//! never steer a later, unrelated launch. Every failure degrades to "no
//! request".
//!
//! [`RequestFile`] works on any directory (tests use a temp dir and never
//! touch the real home); the free functions use [`lock_dir`], the same
//! `~/.localflow` directory as `singleinstance.lock_dir()` (computed at call
//! time from `HOME` / `USERPROFILE`).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::pystr::PyStr;

pub const HUB_PAGES: [&str; 10] = [
    "Home",
    "Ask",
    "Hot Phrases",
    "Dictionary",
    "Prompts",
    "Meetings",
    "History",
    "Style",
    "Models",
    "Settings",
];

const KNOWN_PAGES: [&str; 11] = [
    "Setup",
    "Home",
    "Ask",
    "Hot Phrases",
    "Dictionary",
    "Prompts",
    "Meetings",
    "History",
    "Style",
    "Models",
    "Settings",
];
pub const DEFAULT_PAGE: &str = "Home";
pub const REQUEST_NAME: &str = "page-request.json";
pub const STALE_SECONDS: f64 = 20.0;
const FUTURE_SKEW: f64 = 5.0;
const MAX_BYTES: u64 = 4096;

/// Directory holding lock files and the page request: `~/.localflow`
/// (`singleinstance.lock_dir`). Computed at call time, like Python.
pub fn lock_dir() -> PathBuf {
    lf_core::home::localflow_dir()
}

/// Own the shared Python/Rust Hub lock, or hand the page to its current owner.
pub fn acquire_main_app(
    page: Option<&str>,
) -> std::io::Result<Option<lf_runtime::singleinstance::SingleInstance>> {
    acquire_main_app_at(&lock_dir(), page)
}

fn acquire_main_app_at(
    dir: &Path,
    page: Option<&str>,
) -> std::io::Result<Option<lf_runtime::singleinstance::SingleInstance>> {
    let mut instance = lf_runtime::singleinstance::acquire_at(&dir.join("main-app.lock"))?;
    if let Some(instance) = instance.as_mut() {
        let identity = instance.identity()?;
        let marker = dir.join(format!("main-app.rust.{}.tmp", std::process::id()));
        fs::write(
            &marker,
            json!({"protocol":1,"identity":identity}).to_string(),
        )?;
        fs::rename(marker, dir.join("main-app.rust.json"))?;
    }
    if instance.is_none() && page == Some("Setup") && !rust_owner(dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "LocalFlow is already running; Setup cannot be handed to a possibly Python owner. Run `lf setup` to open standalone setup, or close LocalFlow and retry `lf-hub --setup`.",
        ));
    }
    if instance.is_none()
        && !RequestFile::new(dir).write_request(page.unwrap_or(DEFAULT_PAGE), &system_now)
    {
        return Err(std::io::Error::other(
            "could not hand the page request to the running LocalFlow",
        ));
    }
    Ok(instance)
}

fn rust_owner(dir: &Path) -> bool {
    let Some(marker) = fs::read(dir.join("main-app.rust.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return false;
    };
    let Ok(identity) = fs::read_to_string(dir.join("main-app.lock")) else {
        return false;
    };
    marker["protocol"] == 1 && marker["identity"].as_str() == Some(identity.as_str())
}

#[cfg(all(test, not(target_os = "windows")))]
mod rust_owner_tests {
    use super::*;
    #[test]
    fn setup_handoff_reaches_rust_owner_but_stale_marker_cannot_identify_python() {
        let temp = tempfile::tempdir().unwrap();
        let owner = acquire_main_app_at(temp.path(), None).unwrap().unwrap();
        assert!(acquire_main_app_at(temp.path(), Some("Setup"))
            .unwrap()
            .is_none());
        assert_eq!(
            RequestFile::new(temp.path()).consume_request(&system_now),
            Some("Setup")
        );
        drop(owner);
        let _python = lf_runtime::singleinstance::acquire_at(&temp.path().join("main-app.lock"))
            .unwrap()
            .unwrap();
        assert!(!rust_owner(temp.path()));
        assert_eq!(
            acquire_main_app_at(temp.path(), Some("Setup"))
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }
    #[test]
    fn corrupt_marker_does_not_grant_setup_capability() {
        let temp = tempfile::tempdir().unwrap();
        let _owner = lf_runtime::singleinstance::acquire_at(&temp.path().join("main-app.lock"))
            .unwrap()
            .unwrap();
        fs::write(temp.path().join("main-app.rust.json"), "not json").unwrap();
        assert!(!rust_owner(temp.path()));
    }
}

/// The canonical page name, or `None` when `page` is not a Hub page. Strict:
/// only surrounding whitespace is ignored (`"hot phrases"` is rejected).
pub fn validate_page(page: &str) -> Option<&'static str> {
    let page = page.py_strip();
    KNOWN_PAGES.iter().copied().find(|name| *name == page)
}

/// [`validate_page`] for an untyped JSON value (anything but a string is
/// `None`).
pub fn validate_page_value(page: &Value) -> Option<&'static str> {
    validate_page(page.as_str()?)
}

/// Case-insensitive lookup for typed input (`lf hub --page settings`).
///
/// Only user-facing entry points use this; requests and routing stay on the
/// strict [`validate_page`]. Runs of whitespace collapse to one space.
pub fn canonical_page(page: &str) -> Option<&'static str> {
    let folded = caseless::default_case_fold_str(&page.py_split().join(" "));
    KNOWN_PAGES
        .iter()
        .copied()
        .find(|name| caseless::default_case_fold_str(name) == folded)
}

/// Resolve a request to a page implemented by this stack layer.
pub fn ported_page(page: Option<&str>) -> &'static str {
    page.and_then(validate_page)
        .filter(|name| HUB_PAGES.contains(name))
        .unwrap_or(DEFAULT_PAGE)
}

/// Inspect the raw v2 TOML like Python `main_app.is_configured`: defaults added
/// by [`lf_core::config::load_config`] must not make an empty file configured.
pub fn configuration_marker(data: &Map<String, Value>) -> bool {
    let setup_done = data
        .get("setup")
        .and_then(Value::as_object)
        .and_then(|setup| setup.get("version"))
        .and_then(Value::as_i64)
        .is_some_and(|version| version > 0);
    setup_done
        || data
            .get("stt")
            .and_then(Value::as_object)
            .is_some_and(|stt| {
                ["provider", "model"].into_iter().any(|key| {
                    stt.get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.py_strip().is_empty())
                })
            })
}

/// Whether the real config contains the setup marker or an explicit speech
/// provider/model. Missing, malformed and legacy-unreadable configs are not
/// configured, exactly as in Python.
pub fn is_configured() -> bool {
    use lf_core::config::{
        config_from_v2, config_path, legacy_config_path, migrate_v1, parse_toml_file,
    };

    let path = config_path();
    let parsed = if path.exists() {
        parse_toml_file(&path)
    } else {
        let legacy = legacy_config_path();
        if !legacy.exists() {
            return false;
        }
        parse_toml_file(&legacy).map(|data| migrate_v1(&data).0)
    };
    parsed.is_ok_and(|data| {
        config_from_v2(&data, cfg!(target_os = "macos")).is_ok_and(|_| configuration_marker(&data))
    })
}

/// The first native window to show. An explicit setup launch is handled by the
/// AppKit router separately. Keeping this policy pure makes the three important
/// launch paths testable on every platform: configured, first run, and a
/// malformed/unknown requested route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitialRoute {
    Hub { page: &'static str },
    Setup { desired: Option<&'static str> },
}

pub fn plan_initial_route(
    configured: bool,
    requested: Option<&str>,
) -> Result<InitialRoute, String> {
    let page = match requested {
        Some(page) => {
            Some(validate_page(page).ok_or_else(|| format!("unknown Hub page: {page:?}"))?)
        }
        None => None,
    };
    Ok(if page == Some("Setup") {
        InitialRoute::Setup { desired: None }
    } else if configured {
        InitialRoute::Hub {
            page: page.unwrap_or(DEFAULT_PAGE),
        }
    } else {
        InitialRoute::Setup { desired: page }
    })
}

/// What the native router does for one show request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowAction {
    /// An explicit Setup page request: show setup, route state untouched.
    OpenSetup,
    /// `--setup` launch: show setup; saving it does not auto-open the Hub.
    SetupOnly,
    /// First run: show setup, then the Hub (at `desired`) once configured.
    FirstRun {
        desired: Option<&'static str>,
    },
    Hub {
        page: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowPlan {
    pub action: WindowAction,
    /// The one in-process runtime (dictation, menu bar, local server) starts
    /// with whichever window opens, setup included, so it is already running
    /// when first-run setup finishes. Starting is idempotent and guarded by the
    /// process's single-instance locks, so this never creates a second runtime.
    pub start_runtime: bool,
}

pub fn plan_window(
    setup_page: bool,
    setup_only: bool,
    configured: bool,
    requested: Option<&str>,
) -> Result<WindowPlan, String> {
    let action = if setup_page {
        WindowAction::OpenSetup
    } else if setup_only {
        WindowAction::SetupOnly
    } else {
        match plan_initial_route(configured, requested)? {
            InitialRoute::Setup { desired } => WindowAction::FirstRun { desired },
            InitialRoute::Hub { page } => WindowAction::Hub { page },
        }
    };
    Ok(WindowPlan {
        action,
        start_runtime: true,
    })
}

/// Seconds since the Unix epoch (`time.time()`).
pub fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The page-request file in one directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFile {
    dir: PathBuf,
}

impl RequestFile {
    /// A request file inside `dir` (any directory; tests pass a temp dir).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        RequestFile { dir: dir.into() }
    }

    /// The real location, `~/.localflow` ([`lock_dir`]).
    pub fn in_lock_dir() -> Self {
        Self::new(lock_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `request_path()`.
    pub fn path(&self) -> PathBuf {
        self.dir.join(REQUEST_NAME)
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        self.dir
            .join(format!("{REQUEST_NAME}.{}.{suffix}", std::process::id()))
    }

    /// Atomically leave a request for the running app. `false` on any failure
    /// (unknown page, unwritable directory, failed rename); no temp file is
    /// left behind. `now` is the clock (`time.time`).
    pub fn write_request(&self, page: &str, now: &dyn Fn() -> f64) -> bool {
        let Some(page) = validate_page(page) else {
            return false;
        };
        let path = self.path();
        let tmp = self.sibling("tmp");
        let result = (|| -> std::io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let payload = serde_json::to_vec(&json!({"page": page, "ts": now()}))
                .map_err(std::io::Error::other)?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut handle = options.open(&tmp)?;
            handle.write_all(&payload)?;
            drop(handle);
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return false;
        }
        true
    }

    /// Take (and delete) the pending request; `None` when absent, stale or
    /// bad.
    ///
    /// The file is first renamed to a per-process claim name, so when several
    /// consumers race exactly one of them gets the request.
    pub fn consume_request(&self, now: &dyn Fn() -> f64) -> Option<&'static str> {
        // Windows renames can race through handles opened on the same source.
        // Keep the lock file (not the lock) between calls: unlinking it would
        // let concurrent consumers lock different file objects.
        #[cfg(windows)]
        let _consume_lock = {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.dir.join("page-request.consume.lock"))
                .ok()?;
            file.try_lock().ok()?;
            file
        };
        let claim = self.sibling("claim");
        fs::rename(self.path(), &claim).ok()?;
        let data = (|| -> Option<Value> {
            let mut raw = Vec::new();
            fs::File::open(&claim)
                .ok()?
                .take(MAX_BYTES)
                .read_to_end(&mut raw)
                .ok()?;
            serde_json::from_slice(&raw).ok()
        })();
        let _ = fs::remove_file(&claim);
        let data = data?;
        let data = data.as_object()?;
        // A bool is not a timestamp (Python: `isinstance(stamp, bool)`).
        let stamp = data.get("ts")?.as_f64()?;
        let age = now() - stamp;
        if age > STALE_SECONDS || age < -FUTURE_SKEW {
            return None;
        }
        validate_page_value(data.get("page")?)
    }

    /// Remove any pending request; idempotent.
    pub fn clear_request(&self) {
        let _ = fs::remove_file(self.path());
    }
}

/// `request_path()` in the real lock directory.
pub fn request_path() -> PathBuf {
    RequestFile::in_lock_dir().path()
}

/// `write_request(page)` in the real lock directory, system clock.
pub fn write_request(page: &str) -> bool {
    RequestFile::in_lock_dir().write_request(page, &system_now)
}

/// `consume_request()` in the real lock directory, system clock.
pub fn consume_request() -> Option<&'static str> {
    RequestFile::in_lock_dir().consume_request(&system_now)
}

/// `clear_request()` in the real lock directory.
pub fn clear_request() {
    RequestFile::in_lock_dir().clear_request();
}

/// What the app knows about the Hub window it already has
/// (`main_app.front_or_build`'s `existing` controller).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubWindowState {
    /// The Hub was closed (its window delegate shut it down): never reusable.
    pub closed: bool,
    pub miniaturized: bool,
    pub visible: bool,
    /// The page currently shown.
    pub page: String,
}

/// What to do for a launch, reopen or page request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReopenPlan {
    /// Bring the existing Hub forward (deminiaturizing it first if needed) and
    /// go to `navigate` when a different page was asked for.
    Reuse {
        deminiaturize: bool,
        navigate: Option<&'static str>,
    },
    /// Build a new Hub on `page` and show it.
    Build { page: &'static str },
}

/// Port of `main_app.front_or_build` + `_native_hub`: a miniaturized or
/// visible Hub is reused; a closed one never is (its views and timers are shut
/// down for good), so a new Hub is built. `requested` is the page asked for
/// (validated here; unknown or absent means none).
pub fn plan_reopen(existing: Option<&HubWindowState>, requested: Option<&str>) -> ReopenPlan {
    let page = requested
        .and_then(validate_page)
        .map(|name| ported_page(Some(name)));
    if let Some(hub) = existing {
        if !hub.closed && (hub.miniaturized || hub.visible) {
            // Same page: nothing to do (a rebuild would only reset scrolling).
            let navigate = page.filter(|page| *page != hub.page);
            return ReopenPlan::Reuse {
                deminiaturize: hub.miniaturized,
                navigate,
            };
        }
    }
    ReopenPlan::Build {
        page: page.unwrap_or(DEFAULT_PAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unported_requests_fall_back_to_home() {
        let dir = tempfile::tempdir().unwrap();
        let file = RequestFile::new(dir.path());
        for page in KNOWN_PAGES {
            let expected = if HUB_PAGES.contains(&page) {
                page
            } else {
                "Home"
            };
            assert_eq!(ported_page(Some(page)), expected);
            assert!(file.write_request(page, &|| 100.0));
            let request = file.consume_request(&|| 100.0);
            assert_eq!(ported_page(request), expected);
        }
        assert_eq!(ported_page(None), "Home");
        assert_eq!(ported_page(Some("not a page")), "Home");
        assert_eq!(
            plan_reopen(None, Some("Setup")),
            ReopenPlan::Build { page: "Home" }
        );
    }

    fn window(closed: bool, miniaturized: bool, visible: bool, page: &str) -> HubWindowState {
        HubWindowState {
            closed,
            miniaturized,
            visible,
            page: page.to_string(),
        }
    }

    #[test]
    fn a_closed_hub_is_never_reused() {
        // The window delegate shut the closed Hub down for good: build a new one.
        let closed = window(true, false, true, "Home");
        assert_eq!(
            plan_reopen(Some(&closed), None),
            ReopenPlan::Build { page: "Home" }
        );
        assert_eq!(
            plan_reopen(Some(&closed), Some("history")),
            ReopenPlan::Build { page: "Home" },
            "an unvalidated lower-case name is no request"
        );
        assert_eq!(
            plan_reopen(Some(&window(true, true, false, "Style")), Some("History")),
            ReopenPlan::Build { page: "History" }
        );
    }

    #[test]
    fn first_launch_and_a_hidden_window_build() {
        assert_eq!(plan_reopen(None, None), ReopenPlan::Build { page: "Home" });
        assert_eq!(
            plan_reopen(None, Some("Dictionary")),
            ReopenPlan::Build { page: "Dictionary" }
        );
        // Not closed, but neither visible nor miniaturized (ordered out).
        let hidden = window(false, false, false, "Home");
        assert_eq!(
            plan_reopen(Some(&hidden), Some("Prompts")),
            ReopenPlan::Build { page: "Prompts" }
        );
        assert_eq!(
            plan_reopen(Some(&hidden), Some("Nonsense")),
            ReopenPlan::Build { page: "Home" }
        );
    }

    #[test]
    fn a_live_hub_is_reused_and_navigates_only_to_a_different_page() {
        let live = window(false, false, true, "Home");
        assert_eq!(
            plan_reopen(Some(&live), None),
            ReopenPlan::Reuse {
                deminiaturize: false,
                navigate: None
            }
        );
        assert_eq!(
            plan_reopen(Some(&live), Some("Home")),
            ReopenPlan::Reuse {
                deminiaturize: false,
                navigate: None
            }
        );
        assert_eq!(
            plan_reopen(Some(&live), Some("Style")),
            ReopenPlan::Reuse {
                deminiaturize: false,
                navigate: Some("Style")
            }
        );
        let mini = window(false, true, false, "History");
        assert_eq!(
            plan_reopen(Some(&mini), Some("History")),
            ReopenPlan::Reuse {
                deminiaturize: true,
                navigate: None
            }
        );
    }

    #[test]
    fn canonical_page_folds_python_whitespace() {
        assert_eq!(canonical_page("hot\u{1c}phrases"), Some("Hot Phrases"));
        assert_eq!(canonical_page(" \u{1f}SETTINGS\u{2003}"), Some("Settings"));
        assert_eq!(validate_page("\u{1c}Home\u{1f}"), Some("Home"));
    }

    #[test]
    fn configuration_marker_uses_only_explicit_raw_values() {
        assert!(!configuration_marker(&Map::new()));
        assert!(!configuration_marker(
            &serde_json::from_value(json!({"setup": {"version": 0}})).unwrap()
        ));
        assert!(!configuration_marker(
            &serde_json::from_value(json!({"setup": {"version": true}})).unwrap()
        ));
        assert!(!configuration_marker(
            &serde_json::from_value(json!({"stt": {"provider": "  ", "model": 2}})).unwrap()
        ));
        assert!(configuration_marker(
            &serde_json::from_value(json!({"setup": {"version": 1}})).unwrap()
        ));
        assert!(configuration_marker(
            &serde_json::from_value(json!({"stt": {"model": "base"}})).unwrap()
        ));
    }

    #[test]
    fn initial_router_covers_normal_empty_and_error_routes() {
        assert_eq!(
            plan_initial_route(true, None),
            Ok(InitialRoute::Hub { page: "Home" })
        );
        assert_eq!(
            plan_initial_route(true, Some("History")),
            Ok(InitialRoute::Hub { page: "History" })
        );
        assert_eq!(
            plan_initial_route(false, None),
            Ok(InitialRoute::Setup { desired: None })
        );
        assert_eq!(
            plan_initial_route(false, Some("Hot Phrases")),
            Ok(InitialRoute::Setup {
                desired: Some("Hot Phrases")
            })
        );
        assert!(plan_initial_route(true, Some("not-a-page")).is_err());
        assert!(plan_initial_route(false, Some("not-a-page")).is_err());
    }

    #[test]
    fn runtime_starts_with_first_run_and_explicit_setup_not_only_the_hub() {
        let plan =
            |setup_page: bool, setup_only: bool, configured: bool, requested: Option<&str>| {
                plan_window(setup_page, setup_only, configured, requested).unwrap()
            };
        let cases = [
            (
                plan(false, false, false, None),
                WindowAction::FirstRun { desired: None },
            ),
            (
                plan(false, false, false, Some("History")),
                WindowAction::FirstRun {
                    desired: Some("History"),
                },
            ),
            (plan(false, true, false, None), WindowAction::SetupOnly),
            (
                plan(false, true, true, Some("History")),
                WindowAction::SetupOnly,
            ),
            (plan(true, false, true, None), WindowAction::OpenSetup),
            (plan(true, false, false, None), WindowAction::OpenSetup),
            (
                plan(false, false, true, None),
                WindowAction::Hub { page: "Home" },
            ),
            (
                plan(false, false, true, Some("Models")),
                WindowAction::Hub { page: "Models" },
            ),
        ];
        for (plan, action) in cases {
            assert_eq!(plan.action, action);
            assert!(plan.start_runtime, "{action:?} must start the runtime");
        }
        // Errors still surface instead of opening a window.
        assert!(plan_window(false, false, true, Some("not-a-page")).is_err());
        assert!(plan_window(false, false, false, Some("not-a-page")).is_err());
    }

    fn file() -> (tempfile::TempDir, RequestFile) {
        let tmp = tempfile::tempdir().unwrap();
        let requests = RequestFile::new(tmp.path().join(".localflow"));
        (tmp, requests)
    }

    fn leftovers(requests: &RequestFile) -> Vec<String> {
        let mut names: Vec<String> = match fs::read_dir(requests.dir()) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name != "page-request.consume.lock")
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    fn now() -> f64 {
        system_now()
    }

    fn plant(requests: &RequestFile, payload: &[u8]) {
        fs::create_dir_all(requests.dir()).unwrap();
        fs::write(requests.path(), payload).unwrap();
    }

    #[test]
    fn page_list_and_default() {
        assert_eq!(HUB_PAGES[0], DEFAULT_PAGE);
        assert_eq!(DEFAULT_PAGE, "Home");
        assert_eq!(
            HUB_PAGES,
            [
                "Home",
                "Ask",
                "Hot Phrases",
                "Dictionary",
                "Prompts",
                "Meetings",
                "History",
                "Style",
                "Models",
                "Settings"
            ]
        );
    }

    #[test]
    fn main_app_owner_excludes_duplicates_and_hands_off_requests() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let requests = RequestFile::new(dir);
        let owner = acquire_main_app_at(dir, Some("Home")).unwrap().unwrap();
        assert_eq!(owner.path(), dir.join("main-app.lock"));
        assert!(requests.consume_request(&now).is_none());
        assert!(acquire_main_app_at(dir, Some("History")).unwrap().is_none());
        assert_eq!(requests.consume_request(&now), Some("History"));
        assert!(requests.consume_request(&now).is_none());
        drop(owner);
        assert!(acquire_main_app_at(dir, None).unwrap().is_some());
        let (pid, stamp) = lf_runtime::singleinstance::read_holder_at(&dir.join("main-app.lock"));
        assert_eq!(pid, Some(std::process::id()));
        assert!(chrono::DateTime::parse_from_rfc3339(&stamp.unwrap()).is_ok());
    }

    #[test]
    fn setup_refuses_incompatible_handoff_without_overwriting_valid_request() {
        let temp = tempfile::tempdir().unwrap();
        let owner = lf_runtime::singleinstance::acquire_at(&temp.path().join("main-app.lock"))
            .unwrap()
            .unwrap();
        let requests = RequestFile::new(temp.path());
        assert!(requests.write_request("Home", &now));
        let error = acquire_main_app_at(temp.path(), Some("Setup"))
            .err()
            .expect("Setup must not be handed off");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("lf setup"));
        assert_eq!(requests.consume_request(&now), Some("Home"));
        drop(owner);
        assert!(acquire_main_app_at(temp.path(), Some("Setup"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn no_page_launch_requests_a_window_from_a_closed_owner() {
        let temp = tempfile::tempdir().unwrap();
        let owner = acquire_main_app_at(temp.path(), None).unwrap().unwrap();
        assert!(acquire_main_app_at(temp.path(), None).unwrap().is_none());
        let request = RequestFile::new(temp.path()).consume_request(&now);
        assert_eq!(request, Some(DEFAULT_PAGE));
        let closed = HubWindowState {
            closed: true,
            visible: false,
            miniaturized: false,
            page: "History".into(),
        };
        assert_eq!(
            plan_reopen(Some(&closed), request),
            ReopenPlan::Build { page: "Home" }
        );
        drop(owner);
        assert!(acquire_main_app_at(temp.path(), None).unwrap().is_some());
    }

    #[test]
    fn main_app_lock_and_handoff_errors_are_not_treated_as_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("blocked");
        fs::write(&blocked, "not a directory").unwrap();
        assert!(acquire_main_app_at(&blocked, None).is_err());
        let _owner = acquire_main_app_at(temp.path(), None).unwrap().unwrap();
        fs::create_dir(temp.path().join(REQUEST_NAME)).unwrap();
        assert!(acquire_main_app_at(temp.path(), Some("History")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn python_main_app_owner_excludes_rust_and_receives_the_page() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".localflow");
        let mut python = Command::new("python3")
            .args([
                "-c",
                r#"
import sys
sys.path.insert(0, sys.argv[1])
from localflow.singleinstance import acquire
lock = acquire('main-app')
assert lock is not None
print('locked', flush=True)
sys.stdin.readline()
lock.release()
"#,
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .env("HOME", temp.path())
            .env("LOCALFLOW_PLATFORM", "darwin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(python.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "locked");
        assert!(acquire_main_app_at(&dir, Some("History"))
            .unwrap()
            .is_none());
        assert_eq!(
            RequestFile::new(&dir).consume_request(&now),
            Some("History")
        );
        drop(python.stdin.take());
        assert!(python.wait().unwrap().success());
        assert!(acquire_main_app_at(&dir, None).unwrap().is_some());
    }

    #[test]
    fn every_page_round_trips() {
        let (_tmp, requests) = file();
        for page in HUB_PAGES {
            assert!(requests.write_request(page, &now));
            assert_eq!(requests.consume_request(&now), Some(page));
        }
    }

    #[test]
    fn request_is_atomic_private_and_consumed_once() {
        let (_tmp, requests) = file();
        assert!(requests.write_request("Hot Phrases", &now));
        assert!(requests.path().ends_with(".localflow/page-request.json"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(requests.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(leftovers(&requests), ["page-request.json"]);
        assert_eq!(requests.consume_request(&now), Some("Hot Phrases"));
        assert!(leftovers(&requests).is_empty());
        assert_eq!(requests.consume_request(&now), None);
    }

    #[cfg(windows)]
    #[test]
    fn consumer_lock_is_released_when_its_handle_drops() {
        let (_tmp, requests) = file();
        assert!(requests.write_request("Home", &now));
        assert_eq!(requests.consume_request(&now), Some("Home"));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(requests.dir().join("page-request.consume.lock"))
            .unwrap();
        lock.lock().unwrap();
        assert!(requests.write_request("History", &now));
        assert_eq!(requests.consume_request(&now), None);
        drop(lock);
        assert_eq!(requests.consume_request(&now), Some("History"));
        assert_eq!(requests.consume_request(&now), None);
    }

    #[test]
    fn last_writer_wins_and_only_one_page_is_pending() {
        let (_tmp, requests) = file();
        requests.write_request("History", &now);
        requests.write_request("Hot Phrases", &now);
        assert_eq!(requests.consume_request(&now), Some("Hot Phrases"));
        assert_eq!(requests.consume_request(&now), None);
    }

    #[test]
    fn racing_consumers_get_the_request_exactly_once() {
        let (_tmp, requests) = file();
        requests.write_request("Models", &now);
        let results: Vec<Option<&str>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| requests.consume_request(&now)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        // The claim name is per process, so racing threads may also lose the
        // data to each other's cleanup; the guarantee is "never twice".
        assert!(results.iter().filter(|r| r.is_some()).count() <= 1);
        assert!(results.iter().flatten().all(|page| *page == "Models"));
        assert!(leftovers(&requests).is_empty());
    }

    #[test]
    fn invalid_pages_are_never_written() {
        let (_tmp, requests) = file();
        for page in ["Nope", "", "hot phrases", "Home; rm -rf /"] {
            assert!(!requests.write_request(page, &now), "{page:?}");
        }
        assert!(leftovers(&requests).is_empty());
    }

    #[test]
    fn page_with_surrounding_whitespace_is_canonicalised() {
        let (_tmp, requests) = file();
        assert!(requests.write_request(" Hot Phrases ", &now));
        assert_eq!(requests.consume_request(&now), Some("Hot Phrases"));
    }

    #[test]
    fn malformed_or_unknown_requests_fail_safely_and_are_removed() {
        let ts = now();
        let payloads: Vec<String> = vec![
            "not json {".into(),
            "[]".into(),
            "\"Hot Phrases\"".into(),
            "{}".into(),
            "{\"page\": \"Hot Phrases\"}".into(),
            "{\"page\": \"Hot Phrases\", \"ts\": \"now\"}".into(),
            "{\"page\": \"Hot Phrases\", \"ts\": true}".into(),
            format!("{{\"page\": \"Evil\", \"ts\": {ts}}}"),
            format!("{{\"page\": [\"Home\"], \"ts\": {ts}}}"),
            format!("{{\"page\": \"__import__('os').system('x')\", \"ts\": {ts}}}"),
            String::new(),
        ];
        for payload in payloads {
            let (_tmp, requests) = file();
            plant(&requests, payload.as_bytes());
            assert_eq!(requests.consume_request(&now), None, "{payload}");
            assert!(leftovers(&requests).is_empty(), "{payload}");
        }
    }

    #[test]
    fn non_utf8_request_is_ignored() {
        let (_tmp, requests) = file();
        plant(&requests, &[0xff, 0xfe, 0x00, 0x7b]);
        assert_eq!(requests.consume_request(&now), None);
        assert!(leftovers(&requests).is_empty());
    }

    #[test]
    fn oversized_request_is_ignored() {
        let (_tmp, requests) = file();
        let payload = json!({"page": "Home", "ts": now(), "pad": "x".repeat(100_000)});
        plant(&requests, payload.to_string().as_bytes());
        assert_eq!(requests.consume_request(&now), None);
        assert!(leftovers(&requests).is_empty());
    }

    #[test]
    fn stale_and_future_requests_are_dropped() {
        let (_tmp, requests) = file();
        let at = 10_000.0;
        let clock = || at;
        requests.write_request("Hot Phrases", &|| at - STALE_SECONDS - 1.0);
        assert_eq!(requests.consume_request(&clock), None);
        requests.write_request("Hot Phrases", &|| at + 600.0);
        assert_eq!(requests.consume_request(&clock), None);
        requests.write_request("Hot Phrases", &|| at - 1.0);
        assert_eq!(requests.consume_request(&clock), Some("Hot Phrases"));
        // The boundaries: exactly STALE_SECONDS old / 5 s in the future pass.
        requests.write_request("Home", &|| at - STALE_SECONDS);
        assert_eq!(requests.consume_request(&clock), Some("Home"));
        requests.write_request("Home", &|| at + 5.0);
        assert_eq!(requests.consume_request(&clock), Some("Home"));
    }

    #[test]
    fn write_failure_is_reported_and_leaves_no_tmp_file() {
        let (_tmp, requests) = file();
        // A directory where the request file belongs makes the final rename fail.
        fs::create_dir_all(requests.path().join("blocker")).unwrap();
        assert!(!requests.write_request("Hot Phrases", &now));
        assert_eq!(leftovers(&requests), ["page-request.json"]);
    }

    #[test]
    fn unwritable_directory_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        fs::write(&blocker, "x").unwrap();
        // The directory would have to live below a regular file.
        let requests = RequestFile::new(blocker.join(".localflow"));
        assert!(!requests.write_request("Hot Phrases", &now));
        assert_eq!(requests.consume_request(&now), None);
    }

    #[test]
    fn clear_request_is_idempotent() {
        let (_tmp, requests) = file();
        requests.write_request("Home", &now);
        requests.clear_request();
        requests.clear_request();
        assert!(leftovers(&requests).is_empty());
    }

    #[test]
    fn validate_page_is_a_strict_whitelist() {
        assert_eq!(validate_page(" Hot Phrases "), Some("Hot Phrases"));
        assert_eq!(validate_page("hot phrases"), None);
        assert_eq!(validate_page(""), None);
        assert_eq!(validate_page_value(&Value::Null), None);
        assert_eq!(validate_page_value(&json!(3)), None);
        assert_eq!(validate_page_value(&json!(["Home"])), None);
        assert_eq!(validate_page_value(&json!("Models")), Some("Models"));
    }

    #[test]
    fn canonical_page_is_case_and_space_insensitive() {
        assert_eq!(canonical_page("setup"), Some("Setup"));
        assert_eq!(
            plan_initial_route(true, Some("Setup")),
            Ok(InitialRoute::Setup { desired: None })
        );
        assert_eq!(
            plan_initial_route(false, Some("Setup")),
            Ok(InitialRoute::Setup { desired: None })
        );
        assert_eq!(canonical_page("settings"), Some("Settings"));
        assert_eq!(canonical_page("  hot   PHRASES "), Some("Hot Phrases"));
        assert_eq!(canonical_page("MODELS"), Some("Models"));
        assert_eq!(canonical_page("nope"), None);
        assert_eq!(canonical_page(""), None);
    }

    #[test]
    fn lock_dir_follows_the_home_directory_not_a_fixed_path() {
        let dir = lock_dir();
        assert!(dir.ends_with(".localflow"));
        assert_eq!(request_path(), dir.join(REQUEST_NAME));
    }
}
