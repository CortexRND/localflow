//! One desktop window per mode and data root, with page hand-off.
//!
//! The lock is an OS advisory lock (`lf_runtime::singleinstance`: flock /
//! LockFileEx), held by an open handle for the whole process, so a crashed
//! owner never leaves a stale lock and nothing compares PIDs. The page
//! request reuses `lf_ui::hub_route::RequestFile` (whitelisted pages, atomic
//! tmp + rename, per-consumer claim, 20 s TTL, 4 KiB cap).
//!
//! Namespaces are separate on purpose:
//! - real mode: `~/.localflow/desktop/` — never the legacy Hub's
//!   `~/.localflow/main-app.lock` / `page-request.json`, so `lf-hub` and the
//!   Python Hub keep their own routing until cutover;
//! - sandbox: `<sandbox dir>/desktop-instance/` — inside the sandbox, so a
//!   sandbox can never steer the real window and two sandboxes with
//!   different `--data-dir` are independent. `--sandbox-reset` keeps this
//!   directory (it holds the live lock).

use std::path::{Path, PathBuf};

use lf_runtime::singleinstance::{acquire_at, SingleInstance};
use lf_ui::hub_route::{system_now, RequestFile};

use crate::data::DataRoot;
use crate::page::Page;

pub const LOCK_NAME: &str = "main-app.lock";
/// Real-mode instance directory under `~/.localflow`.
pub const REAL_DIR: &str = "desktop";
/// Sandbox instance directory directly under the sandbox base directory.
pub const SANDBOX_DIR: &str = "desktop-instance";

/// Where a window may be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Page(Page),
    Setup,
}

impl Route {
    /// The `hub_route` page id written to the request file.
    pub fn id(self) -> &'static str {
        match self {
            Route::Page(page) => page.route(),
            Route::Setup => "Setup",
        }
    }

    /// Parse a validated request id.
    pub fn from_id(id: &str) -> Option<Route> {
        match lf_ui::hub_route::validate_page(id)? {
            "Setup" => Some(Route::Setup),
            other => Page::ALL
                .into_iter()
                .find(|p| p.route() == other)
                .map(Route::Page),
        }
    }
}

/// The instance directory for a sandbox base directory.
pub fn sandbox_instance_dir(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join(SANDBOX_DIR)
}

/// The instance directory for a data root.
pub fn instance_dir(root: &DataRoot) -> PathBuf {
    match &root.sandbox_dir {
        Some(dir) => sandbox_instance_dir(dir),
        None => root.localflow_dir.join(REAL_DIR),
    }
}

/// This process owns the window for its data root.
pub struct Instance {
    _lock: SingleInstance,
    requests: RequestFile,
    /// A request consumed while becoming the owner (see [`claim`]).
    pending: std::cell::Cell<Option<Route>>,
}

impl Instance {
    fn new(lock: SingleInstance, requests: RequestFile) -> Self {
        Instance {
            _lock: lock,
            requests,
            pending: std::cell::Cell::new(None),
        }
    }

    /// The next routed request from another launch, if any (stale, invalid
    /// and unknown requests are dropped by `RequestFile`).
    pub fn take_request(&self) -> Option<Route> {
        self.pending.take().or_else(|| {
            self.requests
                .consume_request(&system_now)
                .and_then(Route::from_id)
        })
    }

    pub fn dir(&self) -> &Path {
        self.requests.dir()
    }
}

pub enum Claim {
    Owner(Instance),
    /// Another live window owns this root; the route was handed to it.
    HandedOff,
    /// Another live window owns this root and nothing was handed over.
    Busy,
}

/// Become the owner of `dir`, or hand `route` to the current owner.
/// `route = None` means "don't hand anything over" (used before a sandbox
/// reset, which must not run while another window uses the sandbox).
///
/// Race-free: if the owner exits between our failed lock attempt and the
/// request write, the retry takes the lock and consumes the pending request
/// atomically (ours, or a newer launch's that replaced it) and routes it, so
/// no launch is lost and no two windows run.
pub fn claim(dir: &Path, route: Option<Route>) -> std::io::Result<Claim> {
    claim_with(dir, route, || {})
}

/// The instance directory must really be this root's: not a symlink (or a
/// symlinked lock/request file) that would claim another root's lock or
/// steer its window, and not resolving outside its parent directory.
pub fn check_instance_dir(dir: &Path) -> std::io::Result<()> {
    let refuse = |what: &Path| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to use {} for the window lock: it is a symbolic link or points outside its folder",
                what.display()
            ),
        )
    };
    for path in [
        dir.to_path_buf(),
        dir.join(LOCK_NAME),
        dir.join(lf_ui::hub_route::REQUEST_NAME),
    ] {
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(refuse(&path));
        }
    }
    if dir.exists() {
        let parent = dir.parent().ok_or_else(|| refuse(dir))?;
        let inside = match (dir.canonicalize(), parent.canonicalize()) {
            (Ok(resolved), Ok(base)) => resolved.parent() == Some(base.as_path()),
            _ => false,
        };
        if !inside || !dir.is_dir() {
            return Err(refuse(dir));
        }
    }
    Ok(())
}

/// [`claim`] with a hook between the hand-off write and the retry (tests
/// use it to make the owner exit and another launch write meanwhile).
fn claim_with(dir: &Path, route: Option<Route>, between: impl FnOnce()) -> std::io::Result<Claim> {
    // Before any lock or request operation.
    check_instance_dir(dir)?;
    let path = dir.join(LOCK_NAME);
    let requests = RequestFile::new(dir);
    if let Some(lock) = acquire_at(&path)? {
        return Ok(Claim::Owner(Instance::new(lock, requests)));
    }
    let Some(route) = route else {
        return Ok(Claim::Busy);
    };
    if !requests.write_request(route.id(), &system_now) {
        return Err(std::io::Error::other(
            "couldn't hand the page to the running LocalFlow window",
        ));
    }
    between();
    if let Some(lock) = acquire_at(&path)? {
        // The owner went away meanwhile: we are the window now. Take the
        // latest request (atomic claim) and route it on the first poll.
        let owner = Instance::new(lock, requests);
        let latest = owner
            .requests
            .consume_request(&system_now)
            .and_then(Route::from_id);
        owner.pending.set(latest);
        return Ok(Claim::Owner(owner));
    }
    Ok(Claim::HandedOff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_hands_over_its_page_and_setup() {
        let dir = tempfile::tempdir().unwrap();
        let Claim::Owner(owner) = claim(dir.path(), Some(Route::Page(Page::Home))).unwrap() else {
            panic!("first launch owns");
        };
        assert!(owner.take_request().is_none());
        assert!(matches!(
            claim(dir.path(), Some(Route::Page(Page::Settings))).unwrap(),
            Claim::HandedOff
        ));
        assert_eq!(owner.take_request(), Some(Route::Page(Page::Settings)));
        assert!(owner.take_request().is_none(), "consumed once");
        assert!(matches!(
            claim(dir.path(), Some(Route::Setup)).unwrap(),
            Claim::HandedOff
        ));
        assert_eq!(owner.take_request(), Some(Route::Setup));
        assert!(matches!(claim(dir.path(), None).unwrap(), Claim::Busy));
        assert!(owner.take_request().is_none(), "Busy writes nothing");
        drop(owner);
        assert!(matches!(
            claim(dir.path(), Some(Route::Page(Page::Home))).unwrap(),
            Claim::Owner(_)
        ));
    }

    #[test]
    fn owner_exit_during_handoff_routes_the_newest_request() {
        let dir = tempfile::tempdir().unwrap();
        let Claim::Owner(first) = claim(dir.path(), None).unwrap() else {
            panic!()
        };
        let mut first = Some(first);
        let claimed = claim_with(dir.path(), Some(Route::Page(Page::History)), || {
            // The owner exits, and a newer launch hands off before our retry.
            drop(first.take());
            assert!(RequestFile::new(dir.path()).write_request("Settings", &system_now));
        })
        .unwrap();
        let Claim::Owner(owner) = claimed else {
            panic!("the retry takes over the window");
        };
        assert_eq!(
            owner.take_request(),
            Some(Route::Page(Page::Settings)),
            "newest wins, not lost"
        );
        assert_eq!(owner.take_request(), None);
        assert!(
            !RequestFile::new(dir.path()).path().exists(),
            "consumed, not left to steer later"
        );

        // Without a newer launch, our own request is what gets routed.
        drop(owner);
        let Claim::Owner(holder) = claim(dir.path(), None).unwrap() else {
            panic!()
        };
        let mut holder = Some(holder);
        let Claim::Owner(owner) =
            claim_with(dir.path(), Some(Route::Setup), || drop(holder.take())).unwrap()
        else {
            panic!()
        };
        assert_eq!(owner.take_request(), Some(Route::Setup));
    }

    #[test]
    fn page_aliases_and_invalid_requests() {
        assert_eq!(Route::Page(Page::Snippets).id(), "Hot Phrases");
        assert_eq!(
            Route::from_id("Hot Phrases"),
            Some(Route::Page(Page::Snippets))
        );
        assert_eq!(Route::from_id("Setup"), Some(Route::Setup));
        assert_eq!(Route::from_id("hot phrases"), None, "requests are strict");
        assert_eq!(Route::from_id("../etc"), None);
        for page in Page::ALL {
            assert_eq!(
                Route::from_id(Route::Page(page).id()),
                Some(Route::Page(page))
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let Claim::Owner(owner) = claim(dir.path(), None).unwrap() else {
            panic!()
        };
        std::fs::write(
            dir.path().join(lf_ui::hub_route::REQUEST_NAME),
            r#"{"page":"Nowhere","ts":1e18}"#,
        )
        .unwrap();
        assert!(owner.take_request().is_none());
        std::fs::write(
            dir.path().join(lf_ui::hub_route::REQUEST_NAME),
            format!(r#"{{"page":"Models","ts":{}}}"#, system_now() - 60.0),
        )
        .unwrap();
        assert!(owner.take_request().is_none(), "stale request dropped");
    }

    #[test]
    fn namespaces_are_isolated_from_each_other_and_the_legacy_hub() {
        let base = tempfile::tempdir().unwrap();
        let real = DataRoot::sandbox(&base.path().join("a"));
        let other = DataRoot::sandbox(&base.path().join("b"));
        assert_ne!(instance_dir(&real), instance_dir(&other));
        let Claim::Owner(_a) = claim(&instance_dir(&real), None).unwrap() else {
            panic!()
        };
        assert!(matches!(
            claim(&instance_dir(&other), None).unwrap(),
            Claim::Owner(_)
        ));
        // Real-mode namespace is a subdirectory, never the legacy files.
        let mut root = DataRoot::sandbox(base.path());
        root.sandbox_dir = None;
        root.mode = crate::data::Mode::Real;
        let dir = instance_dir(&root);
        assert_eq!(dir, root.localflow_dir.join("desktop"));
        assert_ne!(
            dir.join(LOCK_NAME),
            root.localflow_dir.join("main-app.lock")
        );
        assert_ne!(
            RequestFile::new(&dir).path(),
            RequestFile::new(&root.localflow_dir).path()
        );
        let legacy =
            lf_runtime::singleinstance::acquire_at(&root.localflow_dir.join("main-app.lock"))
                .unwrap()
                .unwrap();
        assert!(matches!(claim(&dir, None).unwrap(), Claim::Owner(_)));
        drop(legacy);
    }

    /// Concurrent launches are separate processes: exactly one becomes the
    /// window, every other one hands off, and the owner receives a request.
    #[test]
    fn concurrent_launch_processes_produce_exactly_one_owner() {
        const CHILD: &str = "LF_DESKTOP_RACE_CHILD";
        if let Some(dir) = std::env::var_os(CHILD) {
            let code = match claim(Path::new(&dir), Some(Route::Page(Page::History))).unwrap() {
                Claim::Owner(owner) => {
                    // Stay the window long enough for every racer to arrive,
                    // then report whether a hand-off reached us.
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if owner.take_request().is_some() {
                        11
                    } else {
                        10
                    }
                }
                Claim::HandedOff => 20,
                Claim::Busy => 30,
            };
            std::process::exit(code);
        }
        let dir = tempfile::tempdir().unwrap();
        let children: Vec<_> = (0..6)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "controllers::instance::tests::concurrent_launch_processes_produce_exactly_one_owner",
                    ])
                    .env(CHILD, dir.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let codes: Vec<i32> = children
            .into_iter()
            .map(|mut c| c.wait().unwrap().code().unwrap_or(-1))
            .collect();
        let owners = codes.iter().filter(|c| matches!(c, 10 | 11)).count();
        assert_eq!(owners, 1, "exactly one window: {codes:?}");
        assert_eq!(codes.iter().filter(|c| **c == 20).count(), 5, "{codes:?}");
        assert!(
            codes.contains(&11),
            "the owner received a hand-off: {codes:?}"
        );
    }

    #[test]
    fn a_child_process_sees_the_lock_held_for_the_parent_lifetime() {
        const CHILD: &str = "LF_DESKTOP_INSTANCE_CHILD";
        if let Some(dir) = std::env::var_os(CHILD) {
            let outcome = match claim(Path::new(&dir), Some(Route::Page(Page::Prompts))).unwrap() {
                Claim::Owner(_) => 10,
                Claim::HandedOff => 20,
                Claim::Busy => 30,
            };
            std::process::exit(outcome);
        }
        let dir = tempfile::tempdir().unwrap();
        let Claim::Owner(owner) = claim(dir.path(), None).unwrap() else {
            panic!()
        };
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controllers::instance::tests::a_child_process_sees_the_lock_held_for_the_parent_lifetime",
            ])
            .env(CHILD, dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(20), "the child handed off");
        assert_eq!(owner.take_request(), Some(Route::Page(Page::Prompts)));
        drop(owner);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controllers::instance::tests::a_child_process_sees_the_lock_held_for_the_parent_lifetime",
            ])
            .env(CHILD, dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(10), "a released lock is taken over");
    }
}
