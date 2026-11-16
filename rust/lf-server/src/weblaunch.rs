//! Private launch files: neither tokens nor single-use codes go in a URL or
//! browser process arguments. Call from a blocking thread (reqwest is blocking).
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const PAGE: &str = include_str!("../resources/launch.html");

pub(crate) fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub fn launch_dir() -> PathBuf {
    lf_core::config::config_dir().join("launch")
}

fn launch_code(base: &str) -> Option<String> {
    let token = lf_core::apitoken::load().ok()?;
    if token.is_empty() {
        return None;
    }
    let value: serde_json::Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?
        .post(format!("{}/api/launch-code", base.trim_end_matches('/')))
        .header("X-Localflow-Token", token)
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .ok()?;
    value["code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn write_launch_file(directory: &Path, action: &str, code: &str) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let now = SystemTime::now();
    for old in fs::read_dir(directory)? {
        let Ok(old) = old else {
            continue;
        };
        let name = old.file_name().to_string_lossy().into_owned();
        if name.starts_with("open-")
            && name.ends_with(".html")
            && old
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age.as_secs() > crate::auth::LAUNCH_CODE_TTL_SECONDS)
        {
            let _ = fs::remove_file(old.path());
        }
    }
    let path = directory.join(format!(
        "open-{}.html",
        lf_core::apitoken::generate_token()?
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(
        PAGE.replace("__ACTION__", &escape_html(action))
            .replace("__CODE__", &escape_html(code))
            .as_bytes(),
    )?;
    Ok(path)
}

/// A private auto-POST HTML file URI, or the unauthenticated bare URL when the
/// server cannot issue a launch code. The saved API token is only read.
pub fn launch_target(base: &str, path: &str) -> String {
    let action = format!("{}{path}", base.trim_end_matches('/'));
    let Some(code) = launch_code(base) else {
        return action;
    };
    write_launch_file(&launch_dir(), &action, &code)
        .ok()
        .and_then(|p| url::Url::from_file_path(p).ok())
        .map(String::from)
        .unwrap_or(action)
}

/// The opener gets only a file URI (or a bare unauthenticated URL).
pub fn open_web_ui(base: &str, path: &str, opener: impl FnOnce(&str)) {
    opener(&launch_target(base, path));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_file_is_private_escaped_and_reload_safe() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("open-old.html");
        fs::write(&old, "old").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&old).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        let path = write_launch_file(dir.path(), "http://localhost/?a=1&b=2", "<code>\"'").unwrap();
        assert!(!old.exists());
        let page = fs::read_to_string(&path).unwrap();
        assert!(page.contains("?a=1&amp;b=2"));
        assert!(page.contains("&lt;code&gt;&quot;&#x27;"));
        assert!(page.contains("nav.type !== 'navigate'"));
        assert!(page.contains("sessionStorage.setItem"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
