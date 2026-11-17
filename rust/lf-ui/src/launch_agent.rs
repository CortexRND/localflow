//! Resolve the login-agent helper only for an installed native bundle.

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
