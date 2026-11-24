//! Port of `localflow/desktop_launch.py` (the `lf --gpui` launcher) for
//! `lf-rs hub`. Selection: `--gpui`/`--legacy`, then `LOCALFLOW_UI`, then
//! [`DEFAULT_UI`] (GPUI). A GPUI selection opens only an absolute
//! `LOCALFLOW_DESKTOP_BIN` or the package `scripts/install_gpui_preview.sh`
//! owns, after a bounded `--help` probe; anything wrong prints one notice and
//! opens the legacy window once. When GPUI comes only from the default, an
//! unsupported platform, no candidate at all (no override, nothing at the
//! package root), the Setup page or no active config with the setup marker
//! (no legacy flat fallback) opens
//! the legacy window quietly, as Python does: the default covers configured
//! Hub windows only, so first run stays on the legacy wizard.
//! Notices are byte-identical to Python's. `lf-hub` keeps its own
//! copy of the probe in `lf_ui::desktop_launch` (different messages, no
//! fallback), so the shared constants are checked against it in the tests.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{fail, outln, usage, Io};

pub const UI_ENV: &str = "LOCALFLOW_UI";
pub const BIN_ENV: &str = "LOCALFLOW_DESKTOP_BIN";
pub const SANDBOX_ENV: &str = "LOCALFLOW_SANDBOX";

pub const LINUX_ROOT: [&str; 3] = [".local", "share", "localflow-preview"];
pub const LINUX_MANIFEST_NAME: &str = "localflow-preview";
pub const MAC_APP: [&str; 2] = ["Applications", "LocalFlow Preview.app"];
pub const MAC_BUNDLE_ID: &str = "com.cortexrnd.localflow.desktop-preview";
pub const LAUNCH_STATUS_MARKER: &str = "EXIT STATUS (launch-status: 1):";

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const STARTUP_WAIT: Duration = Duration::from_millis(1500);
const MAX_METADATA_BYTES: u64 = 1 << 20;
const MAX_HELP_BYTES: u64 = 64 * 1024;

pub const NOTICE_INVALID_ENV: &str =
    "lf: LOCALFLOW_UI must be gpui or legacy; opening the legacy window.";
pub const NOTICE_PLATFORM: &str =
    "lf: LocalFlow Preview has no package for this platform; opening the legacy window.";
pub const NOTICE_NOT_INSTALLED: &str = "lf: LocalFlow Preview isn't installed \
     (see scripts/install_gpui_preview.sh); opening the legacy window.";
pub const NOTICE_NOT_OWNED: &str = "lf: the LocalFlow Preview install location holds \
     something the preview installer didn't install; opening the legacy window.";
pub const NOTICE_BAD_OVERRIDE: &str = "lf: LOCALFLOW_DESKTOP_BIN must be an absolute path \
     to an executable lf-desktop; opening the legacy window.";
pub const NOTICE_PROBE: &str = "lf: that lf-desktop is too old (update LocalFlow Preview) \
     or isn't LocalFlow Preview; opening the legacy window.";
pub const NOTICE_SPAWN: &str = "lf: couldn't start LocalFlow Preview; opening the legacy window.";
pub const SANDBOX_REFUSAL: &str = "LOCALFLOW_SANDBOX is set, and `lf` opens LocalFlow Preview \
     only on your real data. Run the preview directly instead: lf-desktop --sandbox \
     (Linux: localflow-preview --sandbox; macOS: open -a \"LocalFlow Preview\" --args \
     --sandbox), or use --legacy.";

pub fn notice_early_exit(code: i32) -> String {
    format!("lf: LocalFlow Preview exited during startup (code {code}); opening the legacy window.")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ui {
    Gpui,
    Legacy,
}

/// Python `DEFAULT_UI` (Scope 4b). Rollback: `Ui::Legacy` here and in Python.
pub const DEFAULT_UI: Ui = Ui::Gpui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Macos,
}

/// Python `platform_key()`: no preview package outside Linux and macOS.
pub fn platform_key() -> Option<Platform> {
    if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Platform::Macos)
    } else {
        None
    }
}

pub fn flag_choice(gpui: bool, legacy: bool) -> Result<Option<Ui>> {
    match (gpui, legacy) {
        (true, true) => usage("--gpui and --legacy can't be used together"),
        (true, false) => Ok(Some(Ui::Gpui)),
        (false, true) => Ok(Some(Ui::Legacy)),
        (false, false) => Ok(None),
    }
}

/// What a launch reads from the outside world; [`Launcher::system`] in production.
pub struct Launcher<'a> {
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    pub platform: Option<Platform>,
    pub home: Option<PathBuf>,
    pub probe_timeout: Duration,
    pub startup_wait: Duration,
    /// [`is_configured`] in production; read only for an implicit default.
    pub configured: &'a dyn Fn() -> bool,
}

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// Python `desktop_launch.setup_route` for `lf-rs hub` (which has no
/// `--setup`): the Setup page.
pub fn setup_page(page: Option<&str>) -> bool {
    page.is_some_and(|page| crate::pyfmt::py_strip(page) == "Setup")
}

/// Python `desktop_launch.configured`, read-only: the active config file must
/// exist, load, pass `config_from_v2` and carry [`configuration_marker`].
/// There is deliberately no legacy flat fallback (native checks only the
/// active file, and a legacy migration can be deferred or fail), so a
/// legacy-flat-only config stays on the legacy window, which handles its
/// migration as before; nothing is migrated or written here. Missing,
/// unreadable or malformed configs, and an unresolvable home, are not
/// configured. Warnings are discarded.
pub fn is_configured() -> bool {
    use lf_core::config::{config_from_v2, parse_toml_file, try_config_path};

    let Ok(path) = try_config_path() else {
        return false;
    };
    path.exists()
        && parse_toml_file(&path).is_ok_and(|data| {
            config_from_v2(&data, cfg!(target_os = "macos")).is_ok() && configuration_marker(&data)
        })
}

/// `lf_ui::hub_route::configuration_marker`: `[setup] version > 0`, or an
/// explicit non-blank `stt.provider` / `stt.model` in the raw file.
pub fn configuration_marker(data: &serde_json::Map<String, serde_json::Value>) -> bool {
    use serde_json::Value;
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
                        .is_some_and(|value| !crate::pyfmt::py_strip(value).is_empty())
                })
            })
}

impl Launcher<'static> {
    pub fn system() -> Self {
        Launcher {
            env: &process_env,
            platform: platform_key(),
            home: lf_core::home::python_home().ok(),
            probe_timeout: PROBE_TIMEOUT,
            startup_wait: STARTUP_WAIT,
            configured: &is_configured,
        }
    }
}

impl Launcher<'_> {
    fn var(&self, name: &str) -> String {
        (self.env)(name)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// (choice, notice): flag, then `LOCALFLOW_UI`, then [`DEFAULT_UI`].
    pub fn choose_ui(&self, flag: Option<Ui>) -> (Ui, Option<&'static str>) {
        if let Some(flag) = flag {
            return (flag, None);
        }
        match crate::pyfmt::py_strip(&self.var(UI_ENV))
            .to_lowercase()
            .as_str()
        {
            "" => (DEFAULT_UI, None),
            "legacy" => (Ui::Legacy, None),
            "gpui" => (Ui::Gpui, None),
            _ => (Ui::Legacy, Some(NOTICE_INVALID_ENV)),
        }
    }

    /// Python `implicit_choice`: no flag and a blank `LOCALFLOW_UI`.
    pub fn implicit_choice(&self, flag: Option<Ui>) -> bool {
        flag.is_none() && crate::pyfmt::py_strip(&self.var(UI_ENV)).is_empty()
    }

    /// Python `candidate_absent`, for the implicit default only: an
    /// unsupported platform, or no override and nothing at the package root
    /// (or no absolute home). Anything at the root, owned or not, is a
    /// candidate that [`Self::find_desktop`] then checks and reports.
    pub fn candidate_absent(&self) -> bool {
        let Some(key) = self.platform else {
            return true;
        };
        if (self.env)(BIN_ENV).is_some_and(|value| !value.is_empty()) {
            return false;
        }
        let Some(home) = self.home.as_deref().filter(|home| home.is_absolute()) else {
            return true;
        };
        let parts: &[&str] = match key {
            Platform::Linux => &LINUX_ROOT,
            Platform::Macos => &MAC_APP,
        };
        let root = parts.iter().fold(home.to_path_buf(), |p, s| p.join(s));
        std::fs::symlink_metadata(root).is_err()
    }

    /// `lf-desktop`'s own rule: `LOCALFLOW_SANDBOX` is exactly `1` or `true`.
    pub fn sandbox_requested(&self) -> bool {
        (self.env)(SANDBOX_ENV).is_some_and(|value| value == "1" || value == "true")
    }

    /// The executable to open; an explicit override never falls through.
    pub fn find_desktop(&self) -> Result<PathBuf, &'static str> {
        let Some(key) = self.platform else {
            return Err(NOTICE_PLATFORM);
        };
        if let Some(value) = (self.env)(BIN_ENV).filter(|value| !value.is_empty()) {
            let path = PathBuf::from(value);
            return if path.is_absolute() && executable_file(&path, true) {
                Ok(path)
            } else {
                Err(NOTICE_BAD_OVERRIDE)
            };
        }
        match self.home.as_deref().filter(|home| home.is_absolute()) {
            Some(home) => owned_install(key, home),
            None => Err(NOTICE_NOT_INSTALLED),
        }
    }

    /// Open the selected window: LocalFlow Preview, or `legacy` exactly once.
    /// `page` is the canonical page, `None` for a bare command.
    pub fn launch(
        &self,
        flag: Option<Ui>,
        page: Option<&str>,
        legacy: &mut dyn FnMut(&mut Io<'_>) -> Result<()>,
        io: &mut Io<'_>,
    ) -> Result<()> {
        let (choice, notice) = self.choose_ui(flag);
        if let Some(notice) = notice {
            outln!(io.err, "{notice}")?;
        }
        if choice != Ui::Gpui {
            return legacy(io);
        }
        // The conditional default covers configured Hub windows only: the
        // Setup page and first run stay on the legacy wizard, and nothing
        // installed stays legacy. Cheapest first, as in Python.
        if self.implicit_choice(flag)
            && (setup_page(page) || self.candidate_absent() || !(self.configured)())
        {
            return legacy(io);
        }
        if self.sandbox_requested() {
            return fail(SANDBOX_REFUSAL);
        }
        let found = self.find_desktop().and_then(|exe| {
            if probe(&exe, self.probe_timeout) {
                Ok(exe)
            } else {
                Err(NOTICE_PROBE)
            }
        });
        let notice = match found {
            Ok(exe) => match spawn(&desktop_argv(&exe, page), self.startup_wait) {
                Ok(()) => return Ok(()),
                Err(notice) => notice,
            },
            Err(notice) => notice.to_string(),
        };
        outln!(io.err, "{notice}")?;
        legacy(io)
    }
}

#[cfg(unix)]
fn accessible(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the whole call.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn accessible(_path: &Path) -> bool {
    true
}

/// A regular file the user may execute (`os.access(X_OK)`).
fn executable_file(path: &Path, follow_symlinks: bool) -> bool {
    let meta = if follow_symlinks {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    meta.is_ok_and(|meta| meta.is_file()) && accessible(path)
}

/// Contents of a non-symlink regular file of bounded size.
fn small_regular_file(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_METADATA_BYTES {
        return None;
    }
    let mut bytes = std::fs::read(path).ok()?;
    bytes.truncate(MAX_METADATA_BYTES as usize);
    Some(bytes)
}

fn linux_owned(root: &Path) -> bool {
    small_regular_file(&root.join("MANIFEST.json"))
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|manifest| {
            manifest.get("name").and_then(|v| v.as_str()) == Some(LINUX_MANIFEST_NAME)
        })
}

/// `plistlib.loads`: binary (`bplist00`) or XML starting `<?xml`/`<plist`.
fn plist_dict(raw: &[u8]) -> Option<plist::Dictionary> {
    let value = if raw.starts_with(b"bplist00") {
        plist::Value::from_reader(io::Cursor::new(raw)).ok()?
    } else {
        let text = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
        if !(text.starts_with(b"<?xml") || text.starts_with(b"<plist")) {
            return None;
        }
        plist::Value::from_reader_xml(io::Cursor::new(raw)).ok()?
    };
    value.into_dictionary()
}

fn mac_owned(app: &Path) -> bool {
    small_regular_file(&app.join("Contents").join("Info.plist"))
        .and_then(|raw| plist_dict(&raw))
        .is_some_and(|info| {
            info.get("CFBundleIdentifier").and_then(|v| v.as_string()) == Some(MAC_BUNDLE_ID)
        })
}

/// The preview installer's `lf-desktop`, only if its metadata says it's ours.
pub fn owned_install(key: Platform, home: &Path) -> Result<PathBuf, &'static str> {
    let join =
        |base: &Path, parts: &[&str]| parts.iter().fold(base.to_path_buf(), |p, s| p.join(s));
    let (root, exe, owned): (PathBuf, PathBuf, fn(&Path) -> bool) = match key {
        Platform::Linux => {
            let root = join(home, &LINUX_ROOT);
            (
                root.clone(),
                join(&root, &["bin", "lf-desktop"]),
                linux_owned,
            )
        }
        Platform::Macos => {
            let root = join(home, &MAC_APP);
            let exe = join(&root, &["Contents", "MacOS", "lf-desktop"]);
            (root, exe, mac_owned)
        }
    };
    let Ok(meta) = std::fs::symlink_metadata(&root) else {
        return Err(NOTICE_NOT_INSTALLED);
    };
    if !meta.is_dir() || !owned(&root) {
        return Err(NOTICE_NOT_OWNED);
    }
    if !executable_file(&exe, false) {
        return Err(NOTICE_NOT_INSTALLED);
    }
    Ok(exe)
}

fn py_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// The Python probe's regexes: `\A\s*lf-desktop(?![\w-])`, then lines (split
/// on `\n`) `^[ \t]*--page <PAGE>(?:[ \t]|$)`, `^[ \t]*--setup(?:[ \t]|$)`
/// and `^[ \t]*<marker>[ \t]*$`.
pub fn help_has_interface(text: &str) -> bool {
    let identity = text
        .trim_start_matches(py_whitespace)
        .strip_prefix("lf-desktop")
        .is_some_and(|rest| {
            !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-')
        });
    let blank = |c: char| c == ' ' || c == '\t';
    let has_line = |token: &str| {
        text.split('\n').any(|line| {
            line.trim_start_matches(blank)
                .strip_prefix(token)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(blank))
        })
    };
    let launch_status = text
        .split('\n')
        .any(|line| line.trim_matches(blank) == LAUNCH_STATUS_MARKER);
    identity && has_line("--page <PAGE>") && has_line("--setup") && launch_status
}

/// `exe --help` exits 0 within `timeout` with the lf-desktop interface; a hung
/// probe (and only the probe) is killed.
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
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(MAX_HELP_BYTES).read_to_end(&mut out);
        let _ = tx.send(out);
        let _ = io::copy(&mut stdout, &mut io::sink());
    });
    let Some(status) = wait_for(&mut child, timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(out) => status.success() && help_has_interface(&String::from_utf8_lossy(&out)),
        Err(_) => false,
    }
}

fn wait_for(child: &mut std::process::Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => return None,
        }
    }
}

/// Closed argv: `[exe]` for a bare command, else `[exe, --page, page]`.
pub fn desktop_argv(exe: &Path, page: Option<&str>) -> Vec<OsString> {
    let mut argv = vec![exe.as_os_str().to_owned()];
    if let Some(page) = page {
        argv.extend([OsString::from("--page"), OsString::from(page)]);
    }
    argv
}

/// Python's `returncode`: negative signal numbers for a killed child.
fn returncode(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(1)
}

/// Start the window detached and watch it for `wait`; exit 0 within the watch
/// is a hand-off. The window is never killed; stderr is inherited.
pub fn spawn(argv: &[OsString], wait: Duration) -> Result<(), String> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let Ok(mut child) = command.spawn() else {
        return Err(NOTICE_SPAWN.into());
    };
    match wait_for(&mut child, wait) {
        None => Ok(()),
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(notice_early_exit(returncode(status))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;

    use super::*;
    use crate::testing::FakeClipboard;

    const GOOD_HELP: &str = "lf-desktop — LocalFlow native desktop app (GPUI preview)\n\n\
        OPTIONS:\n    --page <PAGE>      Open a page\n    --setup            Open the setup wizard\n\n\
        EXIT STATUS (launch-status: 1):\n    0   closed or handed off\n";

    type Env = Box<dyn Fn(&str) -> Option<OsString>>;

    fn launcher<'a>(env: &'a Env, platform: Option<Platform>, home: Option<&Path>) -> Launcher<'a> {
        Launcher {
            env: &**env,
            platform,
            home: home.map(Path::to_path_buf),
            probe_timeout: PROBE_TIMEOUT,
            startup_wait: STARTUP_WAIT,
            configured: &configured_yes,
        }
    }

    // Most launch tests are about discovery on an already set-up machine.
    fn configured_yes() -> bool {
        true
    }

    fn configured_no() -> bool {
        false
    }

    fn must_not_read_config() -> bool {
        panic!("the configuration marker was read")
    }

    fn unconfigured<'a>(
        env: &'a Env,
        platform: Option<Platform>,
        home: Option<&Path>,
    ) -> Launcher<'a> {
        Launcher {
            configured: &configured_no,
            ..launcher(env, platform, home)
        }
    }

    fn env_of(pairs: &[(&str, &str)]) -> Env {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        Box::new(move |name| map.get(name).cloned())
    }

    fn env_path(name: &'static str, path: &Path) -> Env {
        let value = path.as_os_str().to_owned();
        Box::new(move |key| (key == name).then(|| value.clone()))
    }

    #[test]
    fn shared_constants_match_lf_ui() {
        let ui = include_str!("../../lf-ui/src/desktop_launch.rs");
        for needle in [
            format!("LAUNCH_STATUS_MARKER: &str = \"{LAUNCH_STATUS_MARKER}\";"),
            format!("LINUX_MANIFEST_NAME: &str = \"{LINUX_MANIFEST_NAME}\";"),
            format!("BIN_ENV: &str = \"{BIN_ENV}\";"),
            format!("LINUX_ROOT: [&str; 3] = {LINUX_ROOT:?};"),
        ] {
            assert!(ui.contains(&needle), "{needle}");
        }
    }

    #[test]
    fn selection_is_flag_then_env_then_gpui_default() {
        assert_eq!(DEFAULT_UI, Ui::Gpui);
        assert!(flag_choice(true, true).is_err());
        assert_eq!(flag_choice(true, false).unwrap(), Some(Ui::Gpui));
        assert_eq!(flag_choice(false, true).unwrap(), Some(Ui::Legacy));
        assert_eq!(flag_choice(false, false).unwrap(), None);
        let pairs_for = |value: Option<&'static str>| -> Vec<(&'static str, &'static str)> {
            value.map(|v| (UI_ENV, v)).into_iter().collect()
        };
        let choose = |flag, value: Option<&'static str>| {
            launcher(&env_of(&pairs_for(value)), None, None).choose_ui(flag)
        };
        let implicit = |flag, value: Option<&'static str>| {
            launcher(&env_of(&pairs_for(value)), None, None).implicit_choice(flag)
        };
        assert_eq!(choose(None, None), (Ui::Gpui, None));
        assert_eq!(choose(None, Some(" GPUI\n")), (Ui::Gpui, None));
        assert_eq!(choose(None, Some("legacy")), (Ui::Legacy, None));
        assert_eq!(choose(None, Some("  ")), (Ui::Gpui, None));
        assert_eq!(
            choose(None, Some("bogus")),
            (Ui::Legacy, Some(NOTICE_INVALID_ENV))
        );
        assert_eq!(choose(Some(Ui::Gpui), Some("bogus")), (Ui::Gpui, None));
        assert_eq!(choose(Some(Ui::Legacy), Some("gpui")), (Ui::Legacy, None));
        // Only the default itself is implicit; an explicit choice never is.
        assert!(implicit(None, None) && implicit(None, Some(" \t\n")));
        assert!(!implicit(None, Some("gpui")) && !implicit(None, Some(" GPUI ")));
        assert!(!implicit(None, Some("legacy")) && !implicit(None, Some("bogus")));
        assert!(!implicit(Some(Ui::Gpui), None) && !implicit(Some(Ui::Legacy), None));
    }

    #[test]
    fn sandbox_is_exactly_one_or_true() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("TRUE", false),
            (" 1", false),
            ("0", false),
            ("", false),
        ] {
            let env = env_of(&[(SANDBOX_ENV, value)]);
            assert_eq!(
                launcher(&env, None, None).sandbox_requested(),
                expected,
                "{value:?}"
            );
        }
    }

    #[test]
    fn help_matches_the_python_probe_regexes() {
        assert!(help_has_interface(GOOD_HELP));
        let mark = format!("{LAUNCH_STATUS_MARKER}\n");
        for near in [
            "lf-desktop-other\n    --page <PAGE> x\n    --setup x\n",
            "lf-desktop\n    --page <PAGE> x\n    --setup-extra x\n",
            "lf-desktop\n    --page <PAGE>S x\n    --setup x\n",
            "lf-desktop_x\n    --page <PAGE> x\n    --setup x\n",
            "lf-desktop\n    --page <PAGE>\r\n    --setup x\n",
        ] {
            assert!(!help_has_interface(&format!("{near}{mark}")), "{near:?}");
        }
        assert!(!help_has_interface(&GOOD_HELP.replace("):", "): later")));
        assert!(!help_has_interface(
            &GOOD_HELP.replace("launch-status: 1", "launch-status: 0")
        ));
        assert!(help_has_interface(&format!(
            "\x1c lf-desktop\n\t--page <PAGE>\n\t--setup\n\t{LAUNCH_STATUS_MARKER} \n"
        )));
        assert!(!help_has_interface(""));
    }

    #[test]
    fn argv_is_closed() {
        let exe = Path::new("lf-desktop");
        assert_eq!(desktop_argv(exe, None), [OsString::from("lf-desktop")]);
        assert_eq!(
            desktop_argv(exe, Some("Hot Phrases")),
            ["lf-desktop", "--page", "Hot Phrases"].map(OsString::from)
        );
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn linux_install(home: &Path, manifest: Option<&[u8]>) -> PathBuf {
        let root = LINUX_ROOT.iter().fold(home.to_path_buf(), |p, s| p.join(s));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        if let Some(manifest) = manifest {
            std::fs::write(root.join("MANIFEST.json"), manifest).unwrap();
        }
        let exe = root.join("bin").join("lf-desktop");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        make_executable(&exe);
        exe
    }

    fn mac_install(home: &Path, plist: &[u8]) -> PathBuf {
        let app = MAC_APP.iter().fold(home.to_path_buf(), |p, s| p.join(s));
        let macos = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(app.join("Contents").join("Info.plist"), plist).unwrap();
        let exe = macos.join("lf-desktop");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        make_executable(&exe);
        exe
    }

    #[test]
    fn find_desktop_reports_each_problem() {
        let none = env_of(&[]);
        let home = tempfile::tempdir().unwrap();
        let linux = Some(Platform::Linux);
        let find =
            |env: &Env, platform, home: Option<&Path>| launcher(env, platform, home).find_desktop();
        assert_eq!(find(&none, None, Some(home.path())), Err(NOTICE_PLATFORM));
        assert_eq!(find(&none, linux, None), Err(NOTICE_NOT_INSTALLED));
        assert_eq!(
            find(&none, linux, Some(Path::new("relative"))),
            Err(NOTICE_NOT_INSTALLED)
        );
        assert_eq!(
            find(&none, linux, Some(home.path())),
            Err(NOTICE_NOT_INSTALLED)
        );
        let missing = home.path().join("missing");
        for value in [Path::new("lf-desktop"), &missing, home.path()] {
            assert_eq!(
                find(&env_path(BIN_ENV, value), linux, Some(home.path())),
                Err(NOTICE_BAD_OVERRIDE)
            );
        }
        let exe = linux_install(home.path(), Some(b"{\"name\": \"localflow-preview\"}"));
        assert_eq!(find(&none, linux, Some(home.path())), Ok(exe));
        for manifest in [
            &b"{\"name\": \"localflow\"}"[..],
            b"{\"name\": \"localflow-preview\"",
            b"[\"localflow-preview\"]",
            b"\xef\xbb\xbf{\"name\": \"localflow-preview\"}",
        ] {
            let other = tempfile::tempdir().unwrap();
            linux_install(other.path(), Some(manifest));
            assert_eq!(
                find(&none, linux, Some(other.path())),
                Err(NOTICE_NOT_OWNED)
            );
        }
        let bare = tempfile::tempdir().unwrap();
        linux_install(bare.path(), None);
        assert_eq!(find(&none, linux, Some(bare.path())), Err(NOTICE_NOT_OWNED));
    }

    #[test]
    fn mac_bundle_needs_our_identifier_in_a_python_readable_plist() {
        let none = env_of(&[]);
        let mac = Some(Platform::Macos);
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleIdentifier".into(), MAC_BUNDLE_ID.into());
        let mut xml = Vec::new();
        plist::Value::Dictionary(dict.clone())
            .to_writer_xml(&mut xml)
            .unwrap();
        let mut binary = Vec::new();
        plist::Value::Dictionary(dict)
            .to_writer_binary(&mut binary)
            .unwrap();
        for raw in [&xml, &binary] {
            let home = tempfile::tempdir().unwrap();
            let exe = mac_install(home.path(), raw);
            assert_eq!(
                launcher(&none, mac, Some(home.path())).find_desktop(),
                Ok(exe)
            );
        }
        let headless = b"<dict><key>CFBundleIdentifier</key>\
            <string>com.cortexrnd.localflow.desktop-preview</string></dict>";
        let other = String::from_utf8(xml.clone())
            .unwrap()
            .replace(MAC_BUNDLE_ID, "com.example.other");
        for raw in [
            &headless[..],
            other.as_bytes(),
            b"<plist><dict><key>CFBundleIdentifier",
        ] {
            let home = tempfile::tempdir().unwrap();
            mac_install(home.path(), raw);
            assert_eq!(
                launcher(&none, mac, Some(home.path())).find_desktop(),
                Err(NOTICE_NOT_OWNED)
            );
        }
    }

    struct Run {
        result: Result<()>,
        err: String,
        legacy: usize,
    }

    fn run_launch(launcher: &Launcher<'_>, flag: Option<Ui>, page: Option<&str>) -> Run {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = Cursor::new(Vec::new());
        let mut clipboard = FakeClipboard::default();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
            clipboard: &mut clipboard,
            server_url: None,
            terminal: false,
        };
        let mut legacy = 0;
        let result = launcher.launch(
            flag,
            page,
            &mut |_| {
                legacy += 1;
                Ok(())
            },
            &mut io,
        );
        assert!(out.is_empty());
        Run {
            result,
            err: String::from_utf8(err).unwrap(),
            legacy,
        }
    }

    #[test]
    fn legacy_and_refusals_never_look_for_a_binary() {
        let none = env_of(&[]);
        let run = run_launch(&launcher(&none, None, None), None, None);
        assert!(run.result.is_ok() && run.err.is_empty() && run.legacy == 1);

        let bogus = env_of(&[(UI_ENV, "bogus")]);
        let run = run_launch(&launcher(&bogus, None, None), None, None);
        assert_eq!(
            (run.err, run.legacy),
            (format!("{NOTICE_INVALID_ENV}\n"), 1)
        );

        let sandbox = env_of(&[(SANDBOX_ENV, "1")]);
        let run = run_launch(&launcher(&sandbox, None, None), Some(Ui::Gpui), None);
        let err = run.result.unwrap_err();
        assert!(
            matches!(err.downcast_ref(), Some(crate::CliError::Failed(m)) if m == SANDBOX_REFUSAL)
        );
        assert_eq!(run.legacy, 0);

        let run = run_launch(&launcher(&none, None, None), Some(Ui::Gpui), None);
        assert_eq!((run.err, run.legacy), (format!("{NOTICE_PLATFORM}\n"), 1));
    }

    fn quiet_legacy(run: Run) {
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!((run.err.as_str(), run.legacy), ("", 1));
    }

    #[test]
    fn implicit_default_without_a_candidate_is_quiet_legacy() {
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("missing-lf-desktop");
        let linux = Some(Platform::Linux);
        let mac = Some(Platform::Macos);
        // Unsupported platform: quiet, even with an override (explicit --gpui
        // still reports NOTICE_PLATFORM, above).
        let over = env_path(BIN_ENV, &missing);
        assert!(launcher(&over, None, Some(home.path())).candidate_absent());
        quiet_legacy(run_launch(
            &launcher(&over, None, Some(home.path())),
            None,
            None,
        ));
        let blank_ui = env_of(&[(UI_ENV, "  ")]);
        let none = env_of(&[]);
        for env in [&none, &blank_ui] {
            for platform in [linux, mac] {
                for dir in [None, Some(Path::new("relative")), Some(home.path())] {
                    assert!(launcher(env, platform, dir).candidate_absent());
                    quiet_legacy(run_launch(&launcher(env, platform, dir), None, None));
                }
            }
        }
        // Explicit GPUI keeps reporting the same absence.
        let run = run_launch(
            &launcher(&none, linux, Some(home.path())),
            Some(Ui::Gpui),
            None,
        );
        assert_eq!(
            (run.err, run.legacy),
            (format!("{NOTICE_NOT_INSTALLED}\n"), 1)
        );
        // A sandbox request with nothing installed keeps its legacy semantics.
        let sandbox = env_of(&[(SANDBOX_ENV, "1")]);
        quiet_legacy(run_launch(
            &launcher(&sandbox, linux, Some(home.path())),
            None,
            None,
        ));
    }

    #[test]
    fn implicit_candidate_problems_are_noticed_once() {
        let none = env_of(&[]);
        let linux = Some(Platform::Linux);
        let mac = Some(Platform::Macos);
        let once = |selected: &Launcher<'_>, notice: &str| {
            let run = run_launch(selected, None, None);
            assert!(run.result.is_ok(), "{:?}", run.result);
            assert_eq!((run.err, run.legacy), (format!("{notice}\n"), 1));
        };
        // A bare root without our manifest, and a plain file at the root.
        let bare = tempfile::tempdir().unwrap();
        linux_install(bare.path(), None);
        assert!(!launcher(&none, linux, Some(bare.path())).candidate_absent());
        once(&launcher(&none, linux, Some(bare.path())), NOTICE_NOT_OWNED);
        let file = tempfile::tempdir().unwrap();
        let root = LINUX_ROOT
            .iter()
            .fold(file.path().to_path_buf(), |p, s| p.join(s));
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        std::fs::write(&root, "not a package").unwrap();
        once(&launcher(&none, linux, Some(file.path())), NOTICE_NOT_OWNED);
        // An owned package without its executable.
        let owned = tempfile::tempdir().unwrap();
        let exe = linux_install(owned.path(), Some(b"{\"name\": \"localflow-preview\"}"));
        std::fs::remove_file(exe).unwrap();
        once(
            &launcher(&none, linux, Some(owned.path())),
            NOTICE_NOT_INSTALLED,
        );
        // A macOS bundle with someone else's identifier.
        let other = tempfile::tempdir().unwrap();
        mac_install(
            other.path(),
            b"<plist><dict><key>CFBundleIdentifier</key><string>x</string></dict></plist>",
        );
        once(&launcher(&none, mac, Some(other.path())), NOTICE_NOT_OWNED);
        // A bad override alone now takes effect, so it is noticed.
        let missing = bare.path().join("missing");
        for value in [Path::new("lf-desktop"), missing.as_path()] {
            let env = env_path(BIN_ENV, value);
            once(&launcher(&env, linux, None), NOTICE_BAD_OVERRIDE);
        }
        // A sandbox request with a candidate is refused before any probe.
        let sandbox: Env = Box::new(|name: &str| match name {
            SANDBOX_ENV => Some("true".into()),
            BIN_ENV => Some("lf-desktop".into()),
            _ => None,
        });
        let run = run_launch(&launcher(&sandbox, linux, None), None, None);
        let err = run.result.unwrap_err();
        assert!(
            matches!(err.downcast_ref(), Some(crate::CliError::Failed(m)) if m == SANDBOX_REFUSAL)
        );
        assert_eq!((run.err.as_str(), run.legacy), ("", 0));
        let sandbox = env_of(&[(SANDBOX_ENV, "1")]);
        let run = run_launch(&launcher(&sandbox, linux, Some(bare.path())), None, None);
        assert!(run.result.is_err() && run.legacy == 0);
        // An explicit rollback never looks at the candidate.
        for (flag, env) in [
            (Some(Ui::Legacy), env_of(&[])),
            (None, env_of(&[(UI_ENV, "legacy")])),
            (None, env_of(&[(UI_ENV, " Legacy "), (SANDBOX_ENV, "1")])),
        ] {
            quiet_legacy(run_launch(
                &launcher(&env, linux, Some(bare.path())),
                flag,
                None,
            ));
        }
    }

    #[test]
    fn configuration_marker_matches_python_is_configured() {
        let marker =
            |text: &str| configuration_marker(&lf_core::config::parse_toml_str(text).unwrap());
        for configured in [
            "[setup]\nversion = 1\n",
            "[setup]\nversion = 3\n",
            "[stt]\nprovider = \"faster-whisper\"\n",
            "[stt]\nmodel = \" base \"\n",
        ] {
            assert!(marker(configured), "{configured:?}");
        }
        for not_configured in [
            "",
            "config_version = 2\n",
            "[setup]\nversion = 0\n",
            "[setup]\nversion = true\n",
            "[setup]\nversion = 1.0\n",
            "[setup]\nversion = \"1\"\n",
            "setup = 1\n",
            "[stt]\nprovider = \" \\t\"\nmodel = \"\"\n",
            "[stt]\nmodel = 1\n",
            "stt = \"faster-whisper\"\n",
        ] {
            assert!(!marker(not_configured), "{not_configured:?}");
        }
        // The copy must keep lf-ui's rule (lf-rs doesn't depend on lf-ui).
        let ui = include_str!("../../lf-ui/src/hub_route.rs").replace("\r\n", "\n");
        for needle in [
            "pub fn configuration_marker(data: &Map<String, Value>) -> bool {",
            ".and_then(|setup| setup.get(\"version\"))\n        .and_then(Value::as_i64)\n        \
             .is_some_and(|version| version > 0);",
            "[\"provider\", \"model\"].into_iter().any(|key| {",
            ".is_some_and(|value| !value.py_strip().is_empty())",
        ] {
            assert!(ui.contains(needle), "{needle}");
        }
    }

    #[test]
    fn implicit_setup_page_and_first_run_stay_quietly_legacy() {
        let linux = Some(Platform::Linux);
        let bare = tempfile::tempdir().unwrap();
        linux_install(bare.path(), None); // a candidate that would be noticed
        let bad: Env = Box::new(|name: &str| (name == BIN_ENV).then(|| "lf-desktop".into()));
        let sandboxed: Env = Box::new(|name: &str| match name {
            BIN_ENV => Some("lf-desktop".into()),
            SANDBOX_ENV => Some("1".into()),
            _ => None,
        });
        let none = env_of(&[]);
        // The Setup page never reads the marker or looks for a binary, even
        // with a candidate and a sandbox request.
        for (env, home) in [(&bad, None), (&sandboxed, None), (&none, Some(bare.path()))] {
            for page in ["Setup", " Setup "] {
                let selected = Launcher {
                    configured: &must_not_read_config,
                    ..launcher(env, linux, home)
                };
                quiet_legacy(run_launch(&selected, None, Some(page)));
            }
        }
        // Nothing installed: quiet before the marker is read.
        let selected = Launcher {
            configured: &must_not_read_config,
            ..launcher(&none, linux, None)
        };
        quiet_legacy(run_launch(&selected, None, Some("Hot Phrases")));
        // First run (no setup marker): every page, even with a candidate.
        for (env, home) in [(&bad, None), (&sandboxed, None), (&none, Some(bare.path()))] {
            for page in [None, Some("Home"), Some("Hot Phrases"), Some("Settings")] {
                quiet_legacy(run_launch(&unconfigured(env, linux, home), None, page));
            }
        }
        // The same candidates once configured: the original notices and refusal.
        let run = run_launch(&launcher(&bad, linux, None), None, Some("Home"));
        assert_eq!(
            (run.err, run.legacy),
            (format!("{NOTICE_BAD_OVERRIDE}\n"), 1)
        );
        let run = run_launch(&launcher(&none, linux, Some(bare.path())), None, None);
        assert_eq!((run.err, run.legacy), (format!("{NOTICE_NOT_OWNED}\n"), 1));
        let run = run_launch(&launcher(&sandboxed, linux, None), None, None);
        assert!(run.result.is_err() && run.legacy == 0);
    }

    #[test]
    fn explicit_choices_never_read_the_setup_marker() {
        let linux = Some(Platform::Linux);
        let bad: Env = Box::new(|name: &str| (name == BIN_ENV).then(|| "lf-desktop".into()));
        let explicit = |env: &Env, flag, page| {
            let selected = Launcher {
                configured: &must_not_read_config,
                ..launcher(env, linux, None)
            };
            run_launch(&selected, flag, page)
        };
        // Explicit GPUI keeps the 4a path, Setup page and first run included.
        for page in [None, Some("Setup"), Some("Home")] {
            let run = explicit(&bad, Some(Ui::Gpui), page);
            assert_eq!(
                (run.err, run.legacy),
                (format!("{NOTICE_BAD_OVERRIDE}\n"), 1),
                "{page:?}"
            );
        }
        let gpui_env: Env = Box::new(|name: &str| match name {
            BIN_ENV => Some("lf-desktop".into()),
            UI_ENV => Some(" gpui ".into()),
            _ => None,
        });
        let run = explicit(&gpui_env, None, Some("Setup"));
        assert_eq!(
            (run.err, run.legacy),
            (format!("{NOTICE_BAD_OVERRIDE}\n"), 1)
        );
        let sandbox = env_of(&[(SANDBOX_ENV, "1")]);
        let run = explicit(&sandbox, Some(Ui::Gpui), Some("Setup"));
        assert!(run.result.is_err() && run.legacy == 0);
        // Explicit legacy reads nothing at all.
        quiet_legacy(explicit(&bad, Some(Ui::Legacy), Some("Setup")));
        let ui_legacy: Env = Box::new(|name: &str| match name {
            BIN_ENV => Some("lf-desktop".into()),
            UI_ENV => Some("legacy".into()),
            SANDBOX_ENV => Some("1".into()),
            _ => None,
        });
        quiet_legacy(explicit(&ui_legacy, None, None));
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        // A script written while another test forks can fail to exec with
        // ETXTBSY, so the tests that write and run scripts take turns.
        static SCRIPTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn serial() -> std::sync::MutexGuard<'static, ()> {
            SCRIPTS.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// A fake `lf-desktop` appending each argv to `record`.
        fn fake(dir: &Path, help: &str, launch_code: i32) -> (PathBuf, PathBuf) {
            let record = dir.join("record");
            let exe = dir.join("lf-desktop");
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = --help ]; then\n\
                 cat <<'HELP'\n{help}HELP\nexit 0\nfi\nexit {launch_code}\n",
                record.display()
            );
            std::fs::write(&exe, script).unwrap();
            make_executable(&exe);
            (exe, record)
        }

        fn wait_lines(record: &Path, count: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let text = std::fs::read_to_string(record).unwrap_or_default();
                let lines: Vec<String> = text.lines().map(String::from).collect();
                if lines.len() >= count || Instant::now() > deadline {
                    return lines;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn hand_off_opens_no_legacy_window() {
            let _serial = serial();
            let dir = tempfile::tempdir().unwrap();
            let (exe, record) = fake(dir.path(), GOOD_HELP, 0);
            let env = env_path(BIN_ENV, &exe);
            let linux = Some(Platform::Linux);
            let run = run_launch(&launcher(&env, linux, None), Some(Ui::Gpui), None);
            assert!(run.result.is_ok(), "{:?}", run.result);
            assert_eq!((run.err.as_str(), run.legacy), ("", 0));
            // Under the GPUI default, a binary override alone takes effect.
            let run = run_launch(&launcher(&env, linux, None), None, Some("Hot Phrases"));
            assert!(run.result.is_ok(), "{:?}", run.result);
            assert_eq!((run.err.as_str(), run.legacy), ("", 0));
            let run = run_launch(&launcher(&env, linux, None), Some(Ui::Gpui), Some("Home"));
            assert!(run.result.is_ok() && run.legacy == 0);
            assert_eq!(
                wait_lines(&record, 6),
                [
                    "--help",
                    "",
                    "--help",
                    "--page Hot Phrases",
                    "--help",
                    "--page Home"
                ]
            );
            // An explicit rollback never runs it, and an unsupported platform
            // ignores it quietly under the default.
            for (flag, env, platform) in [
                (Some(Ui::Legacy), env_path(BIN_ENV, &exe), linux),
                (None, env_path(BIN_ENV, &exe), None),
            ] {
                quiet_legacy(run_launch(&launcher(&env, platform, None), flag, None));
            }
            let ui_legacy: Env = {
                let exe = exe.clone().into_os_string();
                Box::new(move |name: &str| match name {
                    UI_ENV => Some("legacy".into()),
                    BIN_ENV => Some(exe.clone()),
                    _ => None,
                })
            };
            quiet_legacy(run_launch(&launcher(&ui_legacy, linux, None), None, None));
            assert_eq!(wait_lines(&record, 6).len(), 6);
        }

        #[test]
        fn implicit_default_opens_the_owned_package_or_falls_back_once() {
            let _serial = serial();
            let none = env_of(&[]);
            let linux = Some(Platform::Linux);
            let install = |help: &str, code: i32| {
                let home = tempfile::tempdir().unwrap();
                let exe = linux_install(home.path(), Some(b"{\"name\": \"localflow-preview\"}"));
                let (_, record) = fake(exe.parent().unwrap(), help, code);
                (home, record)
            };
            let (home, record) = install(GOOD_HELP, 0);
            let run = run_launch(
                &launcher(&none, linux, Some(home.path())),
                None,
                Some("Models"),
            );
            assert!(run.result.is_ok(), "{:?}", run.result);
            assert_eq!((run.err.as_str(), run.legacy), ("", 0));
            assert_eq!(wait_lines(&record, 2), ["--help", "--page Models"]);

            let (home, record) = install(&GOOD_HELP.replace("    --setup", ""), 0);
            let run = run_launch(&launcher(&none, linux, Some(home.path())), None, None);
            assert_eq!((run.err, run.legacy), (format!("{NOTICE_PROBE}\n"), 1));
            assert_eq!(wait_lines(&record, 1), ["--help"]);

            let (home, record) = install(GOOD_HELP, 2);
            let run = run_launch(&launcher(&none, linux, Some(home.path())), None, None);
            assert_eq!(
                (run.err, run.legacy),
                (format!("{}\n", notice_early_exit(2)), 1)
            );
            assert_eq!(wait_lines(&record, 2), ["--help", ""]);

            // A symlinked package root is a candidate, then refused.
            let (real, record) = install(GOOD_HELP, 0);
            let link = tempfile::tempdir().unwrap();
            let root = LINUX_ROOT
                .iter()
                .fold(link.path().to_path_buf(), |p, s| p.join(s));
            std::fs::create_dir_all(root.parent().unwrap()).unwrap();
            let target = LINUX_ROOT
                .iter()
                .fold(real.path().to_path_buf(), |p, s| p.join(s));
            std::os::unix::fs::symlink(target, &root).unwrap();
            let run = run_launch(&launcher(&none, linux, Some(link.path())), None, None);
            assert_eq!((run.err, run.legacy), (format!("{NOTICE_NOT_OWNED}\n"), 1));
            assert!(wait_lines(&record, 0).is_empty());

            // A good owned package on first run, or for the Setup page, is
            // never probed or launched by the default; explicit GPUI opens it.
            let (home, record) = install(GOOD_HELP, 0);
            for (selected, page) in [
                (unconfigured(&none, linux, Some(home.path())), None),
                (
                    unconfigured(&none, linux, Some(home.path())),
                    Some("Models"),
                ),
                (launcher(&none, linux, Some(home.path())), Some("Setup")),
            ] {
                quiet_legacy(run_launch(&selected, None, page));
            }
            assert!(wait_lines(&record, 0).is_empty());
            let run = run_launch(
                &unconfigured(&none, linux, Some(home.path())),
                Some(Ui::Gpui),
                None,
            );
            assert!(run.result.is_ok() && run.err.is_empty() && run.legacy == 0);
            assert_eq!(wait_lines(&record, 2), ["--help", ""]);
        }

        #[test]
        fn early_exit_bad_probe_and_bad_package_fall_back_once() {
            let _serial = serial();
            let linux = Some(Platform::Linux);
            let dir = tempfile::tempdir().unwrap();
            let (exe, record) = fake(dir.path(), GOOD_HELP, 3);
            let env = env_path(BIN_ENV, &exe);
            let run = run_launch(&launcher(&env, linux, None), Some(Ui::Gpui), Some("Ask"));
            assert_eq!(
                (run.err, run.legacy),
                (format!("{}\n", notice_early_exit(3)), 1)
            );
            assert_eq!(wait_lines(&record, 2), ["--help", "--page Ask"]);

            let old = tempfile::tempdir().unwrap();
            let (exe, record) = fake(old.path(), &GOOD_HELP.replace("    --setup", ""), 0);
            let env = env_path(BIN_ENV, &exe);
            let run = run_launch(&launcher(&env, linux, None), Some(Ui::Gpui), None);
            assert_eq!((run.err, run.legacy), (format!("{NOTICE_PROBE}\n"), 1));
            assert_eq!(wait_lines(&record, 1), ["--help"]);

            let none = env_of(&[]);
            let home = tempfile::tempdir().unwrap();
            let exe = linux_install(home.path(), Some(b"{\"name\": \"localflow-preview\"}"));
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
            let run = run_launch(
                &launcher(&none, linux, Some(home.path())),
                Some(Ui::Gpui),
                None,
            );
            assert_eq!(
                (run.err, run.legacy),
                (format!("{NOTICE_NOT_INSTALLED}\n"), 1)
            );
        }

        #[test]
        fn killed_child_reports_a_negative_code_like_python() {
            let _serial = serial();
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("die");
            std::fs::write(&exe, "#!/bin/sh\nkill -9 $$\n").unwrap();
            make_executable(&exe);
            assert_eq!(
                spawn(&[exe.into_os_string()], STARTUP_WAIT),
                Err(notice_early_exit(-9))
            );
            assert_eq!(
                spawn(&[dir.path().join("missing").into_os_string()], STARTUP_WAIT),
                Err(NOTICE_SPAWN.to_string())
            );
        }
    }
}
