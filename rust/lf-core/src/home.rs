//! Python-compatible home directory resolution.
//!
//! Python resolves `~` with `os.path.expanduser`, which reads `HOME` (falling
//! back to the passwd entry) on Unix and `USERPROFILE` (falling back to
//! `HOMEDRIVE` + `HOMEPATH`) on Windows. `dirs::home_dir()` ignores
//! `USERPROFILE` on Windows (it asks for the profile known folder), so every
//! `~/.localflow/...` path in the port goes through these helpers instead.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

const NO_HOME: &str = "Could not determine home directory.";

/// Python `Path.home()` (`os.path.expanduser("~")`).
#[cfg(not(windows))]
pub fn python_home() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let home = match std::env::var_os("HOME") {
        Some(home) => home.as_bytes().to_vec(),
        None => passwd_home().ok_or_else(|| anyhow!(NO_HOME))?,
    };
    Ok(posix_userhome(home))
}

/// Python `Path.home()` on Windows (`ntpath.expanduser("~")`).
#[cfg(windows)]
pub fn python_home() -> Result<PathBuf> {
    windows_userhome().ok_or_else(|| anyhow!(NO_HOME))
}

/// `~/.localflow` (Python `Path.home() / ".localflow"`); a literal `~`
/// prefix is kept when no home directory can be determined at all.
pub fn localflow_dir() -> PathBuf {
    python_home()
        .unwrap_or_else(|_| PathBuf::from("~"))
        .join(".localflow")
}

/// Python `pathlib.Path(path).expanduser()`: a leading `~` or `~user`
/// component (relative paths only) is replaced by the home directory;
/// an unresolvable home is an error like Python's `RuntimeError`.
pub fn expand_user(path: &Path) -> Result<PathBuf> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Ok(path.to_path_buf());
    };
    let first = first.to_string_lossy();
    let Some(user) = first.strip_prefix('~') else {
        return Ok(path.to_path_buf());
    };
    let mut home = expand_tilde(user)?;
    for component in components {
        home.push(component.as_os_str());
    }
    Ok(home)
}

/// `os.path.expanduser("~" + user)` or an error when Python would leave it
/// unexpanded.
#[cfg(not(windows))]
fn expand_tilde(user: &str) -> Result<PathBuf> {
    if user.is_empty() {
        return python_home();
    }
    let home = passwd_home_for(user).ok_or_else(|| anyhow!("{NO_HOME} (unknown user {user:?})"))?;
    Ok(posix_userhome(home))
}

#[cfg(windows)]
fn expand_tilde(user: &str) -> Result<PathBuf> {
    let home = windows_userhome().ok_or_else(|| anyhow!(NO_HOME))?;
    if user.is_empty() {
        return Ok(home);
    }
    // ntpath.expanduser guesses a sibling profile directory, but only when
    // the current profile directory is named after USERNAME.
    let current = std::env::var_os("USERNAME");
    if current.as_deref() == Some(std::ffi::OsStr::new(user)) {
        return Ok(home);
    }
    if current.is_none() || home.file_name() != current.as_deref() {
        return Err(anyhow!("{NO_HOME} (cannot guess the profile of {user:?})"));
    }
    let parent = home.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(parent.join(user))
}

#[cfg(windows)]
fn windows_userhome() -> Option<PathBuf> {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    let path = std::env::var_os("HOMEPATH")?;
    let mut home = std::env::var_os("HOMEDRIVE").unwrap_or_default();
    home.push(path);
    Some(PathBuf::from(home))
}

/// posixpath.expanduser tail: strip trailing slashes, empty means `/`.
#[cfg(not(windows))]
fn posix_userhome(mut home: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    while home.last() == Some(&b'/') {
        home.pop();
    }
    if home.is_empty() {
        home.push(b'/');
    }
    PathBuf::from(std::ffi::OsString::from_vec(home))
}

/// The passwd entry home directory of the current user.
#[cfg(not(windows))]
pub(crate) fn passwd_home() -> Option<Vec<u8>> {
    passwd_lookup(|entry, buffer, result| {
        // SAFETY: all pointers reference live, correctly sized storage.
        unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                result,
            )
        }
    })
}

#[cfg(not(windows))]
fn passwd_home_for(user: &str) -> Option<Vec<u8>> {
    let name = std::ffi::CString::new(user).ok()?;
    passwd_lookup(|entry, buffer, result| {
        // SAFETY: `name` is NUL-terminated; other pointers reference live storage.
        unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                result,
            )
        }
    })
}

#[cfg(not(windows))]
fn passwd_lookup(
    call: impl Fn(&mut libc::passwd, &mut Vec<libc::c_char>, &mut *mut libc::passwd) -> libc::c_int,
) -> Option<Vec<u8>> {
    use std::ffi::CStr;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: passwd is plain data; an all-zero value is valid.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    loop {
        let status = call(&mut entry, &mut buffer, &mut result);
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            let len = buffer.len() * 2;
            buffer.resize(len, 0);
            continue;
        }
        if status != 0 || result.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        break;
    }
    // SAFETY: the lookup succeeded, so pw_dir is a NUL-terminated string in `buffer`.
    Some(unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvGuard, GLOBAL_LOCK};

    fn sandbox() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn non_tilde_paths_are_unchanged() {
        assert_eq!(expand_user(Path::new("a/b")).unwrap(), PathBuf::from("a/b"));
        assert_eq!(expand_user(Path::new("")).unwrap(), PathBuf::from(""));
        assert_eq!(
            expand_user(Path::new("x~/y")).unwrap(),
            PathBuf::from("x~/y")
        );
        let absolute = sandbox();
        let path = absolute.path().join("~");
        assert_eq!(expand_user(&path).unwrap(), path);
    }

    #[cfg(not(windows))]
    #[test]
    fn home_follows_home_env() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = sandbox();
        let mut env = EnvGuard::new();
        env.set("HOME", format!("{}//", dir.path().display()));
        assert_eq!(python_home().unwrap(), dir.path());
        assert_eq!(localflow_dir(), dir.path().join(".localflow"));
        assert_eq!(expand_user(Path::new("~")).unwrap(), dir.path());
        assert_eq!(
            expand_user(Path::new("~/skills/x")).unwrap(),
            dir.path().join("skills").join("x")
        );
        env.set("HOME", "");
        assert_eq!(python_home().unwrap(), PathBuf::from("/"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unknown_user_is_an_error() {
        let err = expand_user(Path::new("~no-such-localflow-user-xyz/a")).unwrap_err();
        assert!(err
            .to_string()
            .contains("Could not determine home directory"));
    }

    /// Python uses `USERPROFILE`, never the profile known folder.
    #[cfg(windows)]
    #[test]
    fn home_follows_userprofile_not_known_folder() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = sandbox();
        let profile = dir.path().join("profile-sandbox");
        assert_ne!(dirs::home_dir().as_deref(), Some(profile.as_path()));
        let mut env = EnvGuard::new();
        env.set("USERPROFILE", &profile);
        env.set("USERNAME", "profile-sandbox");
        assert_eq!(python_home().unwrap(), profile);
        assert_eq!(localflow_dir(), profile.join(".localflow"));
        assert_eq!(
            expand_user(Path::new("~\\a\\b")).unwrap(),
            profile.join("a").join("b")
        );
        assert_eq!(expand_user(Path::new("~/a")).unwrap(), profile.join("a"));
        assert_eq!(
            expand_user(Path::new("~other\\a")).unwrap(),
            dir.path().join("other").join("a")
        );
        env.set("USERNAME", "someone-else");
        assert!(expand_user(Path::new("~other")).is_err());
        env.remove("USERPROFILE");
        env.set("HOMEDRIVE", "Q:");
        env.set("HOMEPATH", "\\Users\\q");
        assert_eq!(python_home().unwrap(), PathBuf::from("Q:\\Users\\q"));
        env.remove("HOMEPATH");
        assert!(python_home().is_err());
    }
}
