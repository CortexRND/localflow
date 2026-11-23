//! How `lf-hub` on Linux finds and opens LocalFlow Preview (`lf-desktop`).
//!
//! The same contract as the Python `lf --gpui` launcher
//! (`localflow/desktop_launch.py`): an absolute `LOCALFLOW_DESKTOP_BIN`, else
//! only the package `scripts/install_gpui_preview.sh` owns (checked by its
//! `MANIFEST.json`). Never `PATH` or a build tree. A candidate must answer a
//! bounded `--help` probe as an `lf-desktop` with `--page`, `--setup` and the
//! [`LAUNCH_STATUS_MARKER`] line (exit 0 only after a window opened or a
//! hand-off; Scope 3 preview builds returned 0 when no window opened). The
//! argv is closed: `--page <canonical>` or `--setup`, never `--engine` or
//! `--sandbox`. Portable policy only; the Linux `exec` lives in `lf-hub`.

use std::ffi::OsStr;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Explicit `lf-desktop` path (absolute only; never falls through when invalid).
pub const BIN_ENV: &str = "LOCALFLOW_DESKTOP_BIN";
/// The preview installer's Linux package root, relative to `HOME`.
pub const LINUX_ROOT: [&str; 3] = [".local", "share", "localflow-preview"];
/// `name` in that package's `MANIFEST.json`.
pub const LINUX_MANIFEST_NAME: &str = "localflow-preview";
/// How long the `--help` probe may run before it (and only it) is killed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `lf-desktop --help` line that promises the launch exit status: 0 only
/// when a window opened (then closed) or the request was handed to the open
/// window; 1 when no window could open. Older preview builds lack it and are
/// treated as too old. Must match `lf-desktop`'s `USAGE` and the Python probe.
pub const LAUNCH_STATUS_MARKER: &str = "EXIT STATUS (launch-status: 1):";

const MAX_METADATA_BYTES: u64 = 1 << 20;
const MAX_HELP_BYTES: u64 = 64 * 1024;

pub const INSTALL_HINT: &str = "Install LocalFlow Preview with scripts/install_gpui_preview.sh \
     (Linux x86_64 packages), or set LOCALFLOW_DESKTOP_BIN to an absolute lf-desktop path.";

/// Why no `lf-desktop` can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    NotInstalled,
    NotOwned,
    BadOverride,
    NotDesktop,
    IsSelf,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Problem::NotInstalled => "LocalFlow Preview isn't installed",
            Problem::NotOwned => {
                "the LocalFlow Preview install location holds something the preview installer didn't install"
            }
            Problem::BadOverride => {
                "LOCALFLOW_DESKTOP_BIN must be an absolute path to an executable lf-desktop"
            }
            Problem::NotDesktop => {
                "that lf-desktop is too old (update LocalFlow Preview) or isn't LocalFlow Preview"
            }
            Problem::IsSelf => "LOCALFLOW_DESKTOP_BIN points at lf-hub itself",
        })
    }
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn linux_owned(root: &Path) -> bool {
    let manifest = root.join("MANIFEST.json");
    let Ok(meta) = std::fs::symlink_metadata(&manifest) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_METADATA_BYTES {
        return false;
    }
    let Ok(bytes) = std::fs::read(&manifest) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("name")?
                .as_str()
                .map(|name| name == LINUX_MANIFEST_NAME)
        })
        .unwrap_or(false)
}

/// The preview package's `bin/lf-desktop`, only if its manifest says it's ours.
pub fn owned_linux_install(home: &Path) -> Result<PathBuf, Problem> {
    let root: PathBuf = LINUX_ROOT
        .iter()
        .fold(home.to_path_buf(), |p, part| p.join(part));
    let Ok(meta) = std::fs::symlink_metadata(&root) else {
        return Err(Problem::NotInstalled);
    };
    if !meta.is_dir() || !linux_owned(&root) {
        return Err(Problem::NotOwned);
    }
    let exe = root.join("bin").join("lf-desktop");
    match std::fs::symlink_metadata(&exe) {
        Ok(meta) if is_executable(&meta) => Ok(exe),
        _ => Err(Problem::NotInstalled),
    }
}

/// Override first (never falling through when invalid), then the owned package.
pub fn find(override_bin: Option<&OsStr>, home: Option<&Path>) -> Result<PathBuf, Problem> {
    if let Some(value) = override_bin.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return match std::fs::metadata(&path) {
            Ok(meta) if path.is_absolute() && is_executable(&meta) => Ok(path),
            _ => Err(Problem::BadOverride),
        };
    }
    match home.filter(|home| home.is_absolute()) {
        Some(home) => owned_linux_install(home),
        None => Err(Problem::NotInstalled),
    }
}

/// `--help` output of an `lf-desktop` with the `--page` / `--setup` interface
/// and the launch-status promise.
pub fn help_has_interface(text: &str) -> bool {
    let identity = text
        .trim_start()
        .strip_prefix("lf-desktop")
        .is_some_and(|rest| {
            !rest.starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_')
        });
    let has_line = |flag: &str| {
        text.lines().any(|line| {
            line.trim_start()
                .strip_prefix(flag)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        })
    };
    let launch_status = text.lines().any(|line| line.trim() == LAUNCH_STATUS_MARKER);
    identity && has_line("--page <PAGE>") && has_line("--setup") && launch_status
}

/// Run `exe --help` with no stdin, bounded output and a deadline; a hung probe
/// is killed. Nothing else is started.
pub fn probe(exe: &Path, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(exe)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.take(MAX_HELP_BYTES).read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    };
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    match rx.recv_timeout(wait) {
        Ok(out) => status.success() && help_has_interface(&String::from_utf8_lossy(&out)),
        Err(_) => false,
    }
}

/// Closed child argv (after the program): `--setup`, `--page <canonical>` or nothing.
pub fn child_args(page: Option<&str>, setup: bool) -> Vec<String> {
    if setup {
        return vec!["--setup".into()];
    }
    match page.and_then(crate::hub_route::canonical_page) {
        Some(page) => vec!["--page".into(), page.into()],
        None => Vec::new(),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The `lf-desktop` that `lf-hub` (at `current_exe`) should `exec`.
pub fn resolve(
    override_bin: Option<&OsStr>,
    home: Option<&Path>,
    current_exe: Option<&Path>,
    timeout: Duration,
) -> Result<PathBuf, Problem> {
    let exe = find(override_bin, home)?;
    if current_exe.is_some_and(|current| same_file(&exe, current)) {
        return Err(Problem::IsSelf);
    }
    if !probe(&exe, timeout) {
        return Err(Problem::NotDesktop);
    }
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_HELP: &str = "lf-desktop — LocalFlow native desktop app (GPUI preview)\n\n\
        OPTIONS:\n    --page <PAGE>      Open a page\n    --setup            Open the setup wizard\n\n\
        EXIT STATUS (launch-status: 1):\n    0   closed or handed off\n";
    // A Scope 3 preview build: every flag, but no launch-status promise.
    const SCOPE3_HELP: &str = "lf-desktop — LocalFlow native desktop app (GPUI preview)\n\n\
        OPTIONS:\n    --page <PAGE>      Open a page\n    --setup            Open the setup wizard\n\
        \x20   --engine           Run dictation\n    --sandbox          Sandbox\n\
        \x20   --data-dir <DIR>   Sandbox directory\n    --sandbox-reset    Re-seed\n\
        \x20   -h, --help         Help\n    -V, --version      Version\n";

    #[test]
    fn help_needs_identity_page_and_setup() {
        assert!(help_has_interface(GOOD_HELP));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("    --setup", "    --set")
        ));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("--page <PAGE>", "--page")
        ));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("lf-desktop —", "lf-desktopx —")
        ));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("lf-desktop —", "lf-hub —")
        ));
        assert!(!help_has_interface(""));
        assert!(!help_has_interface(SCOPE3_HELP));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("launch-status: 1", "launch-status: 0")
        ));
        assert!(!help_has_interface(&GOOD_HELP.replace("):", "): extra")));
        // Same near misses as the Python launcher's tests (each has the marker).
        for near in [
            "lf-desktop-other\n    --page <PAGE> x\n    --setup x\n",
            "lf-desktop\n    --page <PAGE> x\n    --setup-extra x\n",
            "lf-desktop\n    --page <PAGE>S x\n    --setup x\n",
            "lf-desktop_x\n    --page <PAGE> x\n    --setup x\n",
        ] {
            let near = format!("{near}{LAUNCH_STATUS_MARKER}\n");
            assert!(!help_has_interface(&near), "{near}");
        }
        assert!(help_has_interface(&format!(
            "lf-desktop\n\t--page <PAGE>\n\t--setup\n{LAUNCH_STATUS_MARKER}\n"
        )));
        assert!(!help_has_interface("--setup --page <PAGE>"));
    }

    #[test]
    fn child_args_are_closed_and_canonical() {
        assert!(child_args(None, false).is_empty());
        assert_eq!(
            child_args(Some("hot phrases"), false),
            ["--page", "Hot Phrases"]
        );
        assert_eq!(child_args(Some("History"), true), ["--setup"]);
        assert!(child_args(Some("nope; rm -rf /"), false).is_empty());
    }

    #[test]
    fn missing_home_or_package_is_not_installed() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(find(None, None), Err(Problem::NotInstalled));
        assert_eq!(
            find(None, Some(Path::new("relative"))),
            Err(Problem::NotInstalled)
        );
        assert_eq!(find(None, Some(home.path())), Err(Problem::NotInstalled));
        assert_eq!(
            find(Some(OsStr::new("")), Some(home.path())),
            Err(Problem::NotInstalled)
        );
    }

    fn install(home: &Path, manifest: Option<&[u8]>) -> PathBuf {
        let root = home.join(".local/share/localflow-preview");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        if let Some(manifest) = manifest {
            std::fs::write(root.join("MANIFEST.json"), manifest).unwrap();
        }
        let exe = root.join("bin/lf-desktop");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        exe
    }

    #[test]
    fn only_a_package_with_our_manifest_is_trusted() {
        let home = tempfile::tempdir().unwrap();
        let exe = install(
            home.path(),
            Some(b"{\n  \"name\": \"localflow-preview\",\n  \"version\": \"0.2.2\"\n}\n"),
        );
        assert_eq!(find(None, Some(home.path())), Ok(exe));
        for manifest in [
            &b"{\"name\": \"localflow\"}"[..],
            b"{\"name\": \"localflow-preview\"",
            b"[\"localflow-preview\"]",
            b"\xff\xfe",
        ] {
            let other = tempfile::tempdir().unwrap();
            install(other.path(), Some(manifest));
            assert_eq!(find(None, Some(other.path())), Err(Problem::NotOwned));
        }
        let no_manifest = tempfile::tempdir().unwrap();
        install(no_manifest.path(), None);
        assert_eq!(find(None, Some(no_manifest.path())), Err(Problem::NotOwned));
    }

    #[test]
    fn invalid_override_never_falls_through() {
        let home = tempfile::tempdir().unwrap();
        install(home.path(), Some(b"{\"name\": \"localflow-preview\"}"));
        let missing = home.path().join("missing");
        for value in [
            OsStr::new("lf-desktop"),
            OsStr::new("rel/lf-desktop"),
            missing.as_os_str(),
            home.path().as_os_str(),
        ] {
            assert_eq!(
                find(Some(value), Some(home.path())),
                Err(Problem::BadOverride)
            );
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[test]
        fn symlinked_or_non_executable_package_parts_are_refused() {
            let home = tempfile::tempdir().unwrap();
            let exe = install(home.path(), Some(b"{\"name\": \"localflow-preview\"}"));
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(find(None, Some(home.path())), Err(Problem::NotInstalled));
            std::fs::remove_file(&exe).unwrap();
            let elsewhere = script(home.path(), "other", "exit 0");
            symlink(&elsewhere, &exe).unwrap();
            assert_eq!(find(None, Some(home.path())), Err(Problem::NotInstalled));

            let linked = tempfile::tempdir().unwrap();
            let real = tempfile::tempdir().unwrap();
            install(real.path(), Some(b"{\"name\": \"localflow-preview\"}"));
            std::fs::create_dir_all(linked.path().join(".local/share")).unwrap();
            symlink(
                real.path().join(".local/share/localflow-preview"),
                linked.path().join(".local/share/localflow-preview"),
            )
            .unwrap();
            assert_eq!(find(None, Some(linked.path())), Err(Problem::NotOwned));
        }

        #[test]
        fn probe_accepts_only_the_lf_desktop_interface() {
            let dir = tempfile::tempdir().unwrap();
            let good = script(dir.path(), "good", &format!("printf '%s' '{GOOD_HELP}'"));
            assert!(probe(&good, PROBE_TIMEOUT));
            let old = script(
                dir.path(),
                "old",
                "printf 'lf-desktop\\n    --page <PAGE>  x\\n'",
            );
            assert!(!probe(&old, PROBE_TIMEOUT));
            let scope3 = script(
                dir.path(),
                "scope3",
                &format!("printf '%s' '{SCOPE3_HELP}'"),
            );
            assert!(!probe(&scope3, PROBE_TIMEOUT));
            let failing = script(
                dir.path(),
                "failing",
                &format!("printf '%s' '{GOOD_HELP}'; exit 1"),
            );
            assert!(!probe(&failing, PROBE_TIMEOUT));
            let garbage = script(dir.path(), "garbage", "head -c 4096 /dev/urandom");
            assert!(!probe(&garbage, PROBE_TIMEOUT));
            assert!(!probe(&dir.path().join("missing"), PROBE_TIMEOUT));
        }

        #[test]
        fn hanging_probe_is_killed_within_the_deadline() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let hang = script(
                dir.path(),
                "hang",
                &format!("echo $$ > '{}'; exec sleep 60", pid_file.display()),
            );
            let started = Instant::now();
            assert!(!probe(&hang, Duration::from_millis(1500)));
            assert!(started.elapsed() < Duration::from_secs(10));
            let pid: i32 = std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // Killed and reaped: the pid no longer exists.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        }

        #[test]
        fn resolve_refuses_itself_and_stale_binaries() {
            let dir = tempfile::tempdir().unwrap();
            let me = script(dir.path(), "lf-hub", "exit 2");
            let link = dir.path().join("lf-desktop");
            symlink(&me, &link).unwrap();
            assert_eq!(
                resolve(Some(link.as_os_str()), None, Some(&me), PROBE_TIMEOUT),
                Err(Problem::IsSelf)
            );
            // A wrapper around something else that isn't lf-desktop fails the probe.
            let wrapper = script(
                dir.path(),
                "wrapper",
                &format!("exec '{}' \"$@\"", me.display()),
            );
            assert_eq!(
                resolve(Some(wrapper.as_os_str()), None, Some(&me), PROBE_TIMEOUT),
                Err(Problem::NotDesktop)
            );
            let good = script(
                dir.path(),
                "dir with space $(x)",
                &format!("printf '%s' '{GOOD_HELP}'"),
            );
            assert_eq!(
                resolve(Some(good.as_os_str()), None, Some(&me), PROBE_TIMEOUT),
                Ok(good.clone())
            );
        }
    }
}
