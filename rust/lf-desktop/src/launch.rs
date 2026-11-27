//! Launch ordering: take the window lock for the data root first, then
//! prepare (or reset) the data, so a reset can never delete data a live
//! window is using and a second launch hands its page over instead of
//! opening a second window.

use std::path::Path;

use crate::controllers::instance::{self, Claim, Instance, Request};
use crate::data::DataRoot;

pub enum Launched {
    /// This process owns the window.
    Window(Box<DataRoot>, Instance),
    /// A running window got the route; exit successfully.
    HandedOff,
}

/// `--sandbox` (with `--data-dir` / `--sandbox-reset`). `request` is
/// `None` for `--sandbox-exercise`, which opens no window.
pub fn sandbox(dir: &Path, reset: bool, request: Option<Request>) -> Result<Launched, String> {
    sandbox_with(dir, reset, false, request)
}

/// [`sandbox`], seeding the speaker-label fixture into a new (or reset)
/// directory when `fixture` (`--sandbox-fixture`).
pub fn sandbox_with(
    dir: &Path,
    reset: bool,
    fixture: bool,
    request: Option<Request>,
) -> Result<Launched, String> {
    crate::sandbox::check_dir(dir).map_err(|e| format!("sandbox: {e:#}"))?;
    let inst_dir = instance::sandbox_instance_dir(dir);
    // A reset or an exercise never hands a page over: it must not run while
    // the sandbox is open elsewhere.
    match instance::claim(&inst_dir, request.filter(|_| !reset))
        .map_err(|e| format!("sandbox: {e}"))?
    {
        Claim::Owner(owner) => {
            let root = crate::sandbox::prepare_with(dir, reset, fixture)
                .map_err(|e| format!("sandbox: {e:#}"))?;
            Ok(Launched::Window(Box::new(root), owner))
        }
        Claim::HandedOff => Ok(Launched::HandedOff),
        Claim::Busy => Err(format!(
            "sandbox: {} is open in another LocalFlow window; close it before {}.",
            dir.display(),
            if reset {
                "resetting"
            } else {
                "running --sandbox-exercise"
            }
        )),
    }
}

/// Real mode for an already-resolved root.
pub fn real(root: DataRoot, request: Request) -> Result<Launched, String> {
    match instance::claim(&instance::instance_dir(&root), Some(request))
        .map_err(|e| e.to_string())?
    {
        Claim::Owner(owner) => Ok(Launched::Window(Box::new(root), owner)),
        Claim::HandedOff => Ok(Launched::HandedOff),
        Claim::Busy => Err("LocalFlow is already open.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::instance::Route;
    use crate::page::Page;

    fn window(l: Launched) -> (DataRoot, Instance) {
        match l {
            Launched::Window(root, owner) => (*root, owner),
            Launched::HandedOff => panic!("expected a window"),
        }
    }

    #[test]
    fn reset_is_refused_while_the_sandbox_is_open_and_keeps_its_lock_dir() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("sb");
        let (root, owner) =
            window(sandbox(&dir, false, Some(Route::Page(Page::Home).into())).unwrap());
        lf_core::dictionary::DictionaryStore::new(Some(root.dictionary_path.clone()))
            .add_word("KeepMe")
            .unwrap();
        let err = sandbox(&dir, true, Some(Route::Page(Page::Home).into()))
            .err()
            .unwrap();
        assert!(err.contains("open in another LocalFlow window"), "{err}");
        assert!(
            owner.take_request().is_none(),
            "a refused reset hands nothing over"
        );
        let words =
            lf_core::dictionary::DictionaryStore::new(Some(root.dictionary_path.clone())).words();
        assert!(words.contains(&"KeepMe".to_string()), "live data untouched");

        // A normal second launch hands over.
        assert!(matches!(
            sandbox(&dir, false, Some(Route::Page(Page::Models).into())).unwrap(),
            Launched::HandedOff
        ));
        assert_eq!(owner.take_request(), Some(Route::Page(Page::Models).into()));
        drop(owner);

        // With the window closed the reset runs and keeps the instance dir.
        let (root, _owner) = window(sandbox(&dir, true, Some(Route::Setup.into())).unwrap());
        assert!(dir.join(instance::SANDBOX_DIR).is_dir());
        let words =
            lf_core::dictionary::DictionaryStore::new(Some(root.dictionary_path.clone())).words();
        assert!(!words.contains(&"KeepMe".to_string()), "re-seeded");
    }

    #[test]
    fn exercise_is_refused_while_the_sandbox_is_open_and_hands_nothing_over() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("sb");
        let (_root, owner) =
            window(sandbox(&dir, false, Some(Route::Page(Page::Home).into())).unwrap());
        let err = sandbox(&dir, false, None).err().unwrap();
        assert!(err.contains("open in another LocalFlow window"), "{err}");
        assert!(err.contains("--sandbox-exercise"), "{err}");
        assert_eq!(
            owner.take_request(),
            None,
            "a refused exercise hands nothing over"
        );
        drop(owner);
        assert!(matches!(
            sandbox(&dir, false, None).unwrap(),
            Launched::Window(..)
        ));
    }

    #[test]
    fn foreign_directories_are_refused_before_any_lock_file_is_created() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("precious.txt"), "x").unwrap();
        assert!(sandbox(base.path(), false, Some(Route::Page(Page::Home).into())).is_err());
        assert!(sandbox(base.path(), true, Some(Route::Page(Page::Home).into())).is_err());
        assert!(!base.path().join(instance::SANDBOX_DIR).exists());
        assert!(base.path().join("precious.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_instance_dir_cannot_claim_or_steer_another_root() {
        let base = tempfile::tempdir().unwrap();
        // A fake "real root" owner (temp only) and another sandbox's owner.
        let fake_real = base.path().join("real-localflow").join(instance::REAL_DIR);
        let Claim::Owner(real_owner) = instance::claim(&fake_real, None).unwrap() else {
            panic!()
        };
        let a = base.path().join("a");
        let (_a_root, a_owner) =
            window(sandbox(&a, false, Some(Route::Page(Page::Home).into())).unwrap());
        for target in [fake_real.clone(), a.join(instance::SANDBOX_DIR)] {
            let name = target.file_name().unwrap().to_string_lossy().into_owned();
            let evil = base.path().join(format!("evil-{name}"));
            std::fs::create_dir_all(&evil).unwrap();
            std::os::unix::fs::symlink(&target, evil.join(instance::SANDBOX_DIR)).unwrap();
            let err = sandbox(&evil, false, Some(Route::Page(Page::Models).into()))
                .err()
                .unwrap();
            assert!(err.contains("symbolic link"), "{err}");
            assert!(
                sandbox(&evil, true, Some(Route::Setup.into())).is_err(),
                "reset refused too"
            );
        }
        assert_eq!(
            real_owner.take_request(),
            None,
            "no request reached the other root"
        );
        assert_eq!(a_owner.take_request(), None);
        // A symlinked lock file inside a real directory is refused as well.
        let b = base.path().join("b");
        std::fs::create_dir_all(b.join(instance::SANDBOX_DIR)).unwrap();
        std::os::unix::fs::symlink(
            fake_real.join(instance::LOCK_NAME),
            b.join(instance::SANDBOX_DIR).join(instance::LOCK_NAME),
        )
        .unwrap();
        assert!(sandbox(&b, false, Some(Route::Page(Page::Home).into())).is_err());
        assert_eq!(real_owner.take_request(), None);
    }

    #[test]
    fn separate_sandboxes_are_independent_windows() {
        let base = tempfile::tempdir().unwrap();
        let (_a_root, _a) = window(
            sandbox(
                &base.path().join("a"),
                false,
                Some(Route::Page(Page::Home).into()),
            )
            .unwrap(),
        );
        let (_b_root, _b) = window(
            sandbox(
                &base.path().join("b"),
                false,
                Some(Route::Page(Page::Home).into()),
            )
            .unwrap(),
        );
    }
}
