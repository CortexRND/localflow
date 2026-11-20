//! Resolve the login-agent helper only for an installed native bundle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn uninstall(path: &Path, bootout: impl FnOnce()) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    bootout();
    Ok(())
}

/// Opt out of start on login without stopping the running job: remove the
/// agent file, then `disable` its label so launchd skips it at the next login.
/// Unlike [`uninstall`] this never boots the job out, because the job may be
/// the calling app. A failed `disable` is reported, but the file is gone.
pub fn disable_autostart(
    path: &Path,
    disable: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    disable()
}

/// Arguments for the bundled helper enabling start on login from the running
/// app. Passing its PID makes the helper keep that process running and defer
/// loading the agent to the next login, so no second copy starts now.
pub fn install_rust_args(helper: &Path, exe: &Path, running_pid: u32) -> Vec<OsString> {
    vec![
        helper.into(),
        "install-rust".into(),
        exe.into(),
        running_pid.to_string().into(),
    ]
}

/// Arguments for the bundled helper's non-stopping opt-out.
pub fn disable_autostart_args(helper: &Path) -> Vec<OsString> {
    vec![helper.into(), "disable-autostart".into()]
}

pub fn bundled_helper(exe: &Path) -> Option<PathBuf> {
    if exe.file_name()? != "lf-hub" || exe.parent()?.file_name()? != "MacOS" {
        return None;
    }
    let contents = exe.parent()?.parent()?;
    if contents.file_name()? != "Contents" || contents.parent()?.extension()? != "app" {
        return None;
    }
    if !contents.join("Info.plist").is_file() {
        return None;
    }
    Some(contents.join("Resources/macos_launchagent.sh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_removes_plist_before_bootout_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.plist");
        std::fs::write(&path, "plist").unwrap();
        for _ in 0..2 {
            let called = std::cell::Cell::new(false);
            uninstall(&path, || {
                assert!(!path.exists());
                called.set(true);
            })
            .unwrap();
            assert!(called.get());
        }
    }

    #[test]
    fn uninstall_does_not_bootout_when_plist_removal_fails() {
        let root = tempfile::tempdir().unwrap();
        assert!(uninstall(root.path(), || panic!("bootout before successful removal")).is_err());
    }

    #[test]
    fn disable_autostart_removes_plist_then_disables_without_bootout() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.plist");
        std::fs::write(&path, "plist").unwrap();
        for _ in 0..2 {
            let disabled = std::cell::Cell::new(false);
            disable_autostart(&path, || {
                assert!(!path.exists(), "the agent file goes first");
                disabled.set(true);
                Ok(())
            })
            .unwrap();
            assert!(disabled.get());
        }
    }

    #[test]
    fn disable_autostart_reports_failures_and_skips_disable_when_removal_fails() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.plist");
        std::fs::write(&path, "plist").unwrap();
        let error = disable_autostart(&path, || Err("launchctl denied".into())).unwrap_err();
        assert_eq!(error, "launchctl denied");
        assert!(!path.exists());
        // A directory cannot be removed as a file: nothing is disabled.
        assert!(disable_autostart(root.path(), || panic!("disabled before removal")).is_err());
    }

    #[test]
    fn helper_arguments_keep_the_running_app_alive() {
        let helper = Path::new("/A.app/Contents/Resources/macos_launchagent.sh");
        let exe = Path::new("/A.app/Contents/MacOS/lf-hub");
        assert_eq!(
            install_rust_args(helper, exe, 4242),
            vec![
                OsString::from(helper),
                "install-rust".into(),
                exe.into(),
                "4242".into()
            ]
        );
        assert_eq!(
            disable_autostart_args(helper),
            vec![OsString::from(helper), "disable-autostart".into()]
        );
    }

    #[test]
    fn only_native_bundle_layout_selects_the_bundled_helper() {
        let root = tempfile::tempdir().unwrap();
        let contents = root.path().join("Application & Test.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        let exe = contents.join("MacOS/lf-hub");
        assert_eq!(bundled_helper(&exe), None);
        std::fs::write(contents.join("Info.plist"), "plist").unwrap();
        assert_eq!(
            bundled_helper(&exe),
            Some(contents.join("Resources/macos_launchagent.sh"))
        );
        assert_eq!(bundled_helper(&contents.join("MacOS/localflow")), None);
        assert_eq!(bundled_helper(&root.path().join("release/lf-hub")), None);
        assert_eq!(bundled_helper(Path::new("lf-hub")), None);
    }
}
