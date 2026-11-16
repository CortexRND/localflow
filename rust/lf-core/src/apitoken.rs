//! Port of `localflow/apitoken.py`: the local API bearer token file.
//!
//! [`read`] and [`load`] never create, write or rotate the token.
//! [`load_or_create`] mirrors Python exactly: every call issues a fresh token
//! (32 random bytes, URL-safe base64 without padding) and overwrites the file
//! with owner-only permissions. File-system errors while writing are ignored,
//! as in Python, so the server still gets a usable in-memory token.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use base64::Engine as _;

use crate::config::{py_strip, try_config_dir};

/// `config_dir()/api-token`. Panics like [`crate::config::config_dir`].
pub fn token_path() -> PathBuf {
    try_token_path().unwrap_or_else(|err| panic!("{err:#}"))
}

/// Fallible [`token_path`].
pub fn try_token_path() -> Result<PathBuf> {
    Ok(try_config_dir()?.join("api-token"))
}

/// Python `read()`: the stripped token, or `None` when the file is missing,
/// unreadable or blank. Invalid UTF-8 is an error, as in Python.
pub fn read() -> Result<Option<String>> {
    read_at(&try_token_path()?)
}

/// [`read`] for an explicit token file.
pub fn read_at(path: &Path) -> Result<Option<String>> {
    let token = load_at(path)?;
    Ok(if token.is_empty() { None } else { Some(token) })
}

/// Python `load()`: the stripped token without rotating it, or "" when the
/// file is missing or unreadable. Invalid UTF-8 is an error, as in Python.
pub fn load() -> Result<String> {
    load_at(&try_token_path()?)
}

/// [`load`] for an explicit token file.
pub fn load_at(path: &Path) -> Result<String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(String::new()),
    };
    let text = String::from_utf8(bytes).map_err(|err| {
        anyhow!(
            "{}: 'utf-8' codec can't decode the token: {err}",
            path.display()
        )
    })?;
    // Text-mode universal newlines, then str.strip().
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Ok(py_strip(&text).to_string())
}

/// A new token: `secrets.token_urlsafe(32)`.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| anyhow!("os random source failed: {err}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Python `load_or_create()`: issue a new token, write it to [`token_path`]
/// (best effort) and return it. Only a failing OS random source is an error.
pub fn load_or_create() -> Result<String> {
    let token = generate_token()?;
    if let Ok(path) = try_token_path() {
        let _ = write_token(&path, &token);
    }
    Ok(token)
}

/// [`load_or_create`] for an explicit token file.
pub fn load_or_create_at(path: &Path) -> Result<String> {
    let token = generate_token()?;
    let _ = write_token(path, &token);
    Ok(token)
}

fn write_token(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // Python write_text uses text mode (CRLF on Windows).
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    file.write_all(format!("{token}{newline}").as_bytes())?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // An existing file keeps its old mode on open; tighten it like chmod(0o600).
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_overwrites_token_with_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");

        let first = load_or_create_at(&path).unwrap();
        let second = load_or_create_at(&path).unwrap();

        assert_ne!(first, second);
        let newline = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{second}{newline}")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn load_or_create_tightens_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        load_or_create_at(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn load_or_create_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("api-token");
        let token = load_or_create_at(&path).unwrap();
        assert_eq!(load_at(&path).unwrap(), token);
    }

    #[test]
    fn load_or_create_ignores_write_errors() {
        let dir = tempfile::tempdir().unwrap();
        // The parent "directory" is a regular file, so writing must fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "x").unwrap();
        let token = load_or_create_at(&blocker.join("api-token")).unwrap();
        assert_eq!(token.len(), 43);
        assert!(!blocker.join("api-token").exists());
    }

    #[test]
    fn token_is_43_url_safe_characters() {
        let token = generate_token().unwrap();
        assert_eq!(token.len(), 43);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn read_returns_token_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");

        assert_eq!(read_at(&path).unwrap(), None);
        fs::write(&path, "token-value\n").unwrap();
        assert_eq!(read_at(&path).unwrap().as_deref(), Some("token-value"));
        fs::write(&path, "  \n").unwrap();
        assert_eq!(read_at(&path).unwrap(), None);
    }

    #[test]
    fn load_reads_existing_token_without_changing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        let token = load_or_create_at(&path).unwrap();
        let contents = fs::read(&path).unwrap();

        assert_eq!(load_at(&path).unwrap(), token);
        assert_eq!(read_at(&path).unwrap().as_deref(), Some(token.as_str()));
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn load_returns_empty_when_missing_and_never_creates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("api-token");
        assert_eq!(load_at(&path).unwrap(), "");
        assert_eq!(read_at(&path).unwrap(), None);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn load_treats_unreadable_path_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        // A directory cannot be read as a file: an OSError in Python.
        assert_eq!(load_at(dir.path()).unwrap(), "");
        assert_eq!(read_at(dir.path()).unwrap(), None);
    }

    #[test]
    fn invalid_utf8_is_an_error_like_python() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(load_at(&path).is_err());
        assert!(read_at(&path).is_err());
    }

    #[test]
    fn load_strips_whitespace_and_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-token");
        fs::write(&path, "\t abc \r\n").unwrap();
        assert_eq!(load_at(&path).unwrap(), "abc");
    }
}
