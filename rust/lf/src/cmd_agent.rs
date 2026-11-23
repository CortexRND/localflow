//! `agent install|uninstall|status`: the hidden Python `lf agent` group
//! (`cli._autostart_*`) over `localflow.platform.current().autostart_*`:
//! a freedesktop autostart entry on Linux (`platform/linux.py`), a launchd
//! LaunchAgent on macOS (`launchagent.py`, with `logfiles.trim_in_place`
//! for its logs) and the HKCU `Run` value on Windows (`platform/win32.py`).

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};
use lf_core::home::python_home;

use crate::args::AgentCommand;
use crate::pyfmt::{plist_xml, py_strip, str_repr, Plist};
use crate::pyfs::{mkdir_parents, unlink, write_text, PyOsError};
use crate::richfmt::Console;
use crate::{python, CliError, Io};

const LABEL: &str = "com.cortexrnd.localflow";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const KEEP_BYTES: u64 = MAX_LOG_BYTES / 2;
const CHUNK_BYTES: u64 = 1024 * 1024;
const AGENT_LOGS: [&str; 2] = ["agent.out.log", "agent.err.log"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Platform {
    Darwin,
    Linux,
    Win32,
}

/// A Python `RuntimeError` the CLI reports as `error: …`.
#[derive(Debug)]
struct RuntimeError(String);

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuntimeError {}

pub(crate) fn run(command: AgentCommand, io: &mut Io<'_>) -> Result<()> {
    let console = Console::detect();
    let platform = current()?;
    let lines = match command {
        AgentCommand::Install => {
            let result = match platform {
                Platform::Darwin => darwin_install(io),
                Platform::Linux => linux_install(),
                Platform::Win32 => win32::install(),
            };
            let path = caught(result, io, &console)?;
            let path = path.to_string_lossy();
            if platform == Platform::Darwin {
                vec![
                    format!("[green]installed[/green] {path}"),
                    "localflow starts on login and relaunches if it crashes; \
                     a clean exit stays down until next login (KeepAlive)."
                        .to_string(),
                ]
            } else {
                vec![
                    format!("installed: {path}"),
                    "localflow will start at login".to_string(),
                ]
            }
        }
        AgentCommand::Uninstall => {
            let result = match platform {
                Platform::Darwin => darwin_uninstall(),
                Platform::Linux => linux_uninstall(),
                Platform::Win32 => win32::uninstall(),
            };
            caught(result, io, &console)?;
            vec!["[green]uninstalled[/green]".to_string()]
        }
        AgentCommand::Status => {
            let result = match platform {
                Platform::Darwin => darwin_status(),
                Platform::Linux => linux_status(),
                Platform::Win32 => win32::status(),
            };
            vec![caught(result, io, &console)?]
        }
    };
    for line in lines {
        write!(io.out, "{}", console.markup(&line))?;
    }
    Ok(())
}

/// `except (RuntimeError, OSError) as exc`: print `error: {exc}`, exit 1.
fn caught<T>(result: Result<T>, io: &mut Io<'_>, console: &Console) -> Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let message = if let Some(os) = error.downcast_ref::<PyOsError>() {
        os.to_string()
    } else if let Some(runtime) = error.downcast_ref::<RuntimeError>() {
        runtime.to_string()
    } else {
        return Err(error);
    };
    write!(
        io.out,
        "{}",
        console.markup(&format!("[red]error:[/red] {message}"))
    )?;
    Err(CliError::Exit(1).into())
}

/// `localflow.platform.current()`: `LOCALFLOW_PLATFORM`, else the real OS.
fn current() -> Result<Platform> {
    let Some(selected) = env::var_os("LOCALFLOW_PLATFORM") else {
        return Ok(if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Linux
        });
    };
    match selected.to_str() {
        Some("darwin") => Ok(Platform::Darwin),
        Some("linux") => Ok(Platform::Linux),
        Some("win32") => Ok(Platform::Win32),
        _ => bail!(
            "unknown platform: {}",
            str_repr(&selected.to_string_lossy())
        ),
    }
}

// --- linux ------------------------------------------------------------------

/// `platform.linux._desktop_exec`: the program quoted for an `Exec=` key.
fn desktop_exec(program: &str) -> String {
    let escaped = program
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

/// `platformdirs.user_config_dir() / "autostart" / "localflow.desktop"`.
fn autostart_path() -> Result<PathBuf> {
    // lf-core resolves `user_config_dir("localflow")`; without an app name
    // platformdirs returns its parent.
    let config = lf_core::config::try_config_dir()?;
    let base = config.parent().unwrap_or(&config);
    Ok(base.join("autostart").join("localflow.desktop"))
}

fn linux_install() -> Result<PathBuf> {
    let program = python::sibling("localflow");
    let path = autostart_path()?;
    if let Some(parent) = path.parent() {
        mkdir_parents(parent)?;
    }
    write_text(
        &path,
        &format!(
            "[Desktop Entry]\nType=Application\nName=localflow\nExec={}\n\
             X-GNOME-Autostart-enabled=true\n",
            desktop_exec(&program.to_string_lossy())
        ),
    )?;
    Ok(path)
}

fn linux_uninstall() -> Result<()> {
    let path = autostart_path()?;
    if path.exists() {
        unlink(&path)?;
    }
    Ok(())
}

fn linux_status() -> Result<String> {
    let path = autostart_path()?;
    Ok(if path.exists() {
        format!("installed ({})", path.to_string_lossy())
    } else {
        "not installed".to_string()
    })
}

// --- darwin -----------------------------------------------------------------

fn log_dir() -> Result<PathBuf> {
    Ok(python_home()?.join(".localflow"))
}

fn plist_path() -> Result<PathBuf> {
    Ok(python_home()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(unix)]
fn uid() -> Result<u32> {
    // SAFETY: getuid has no preconditions and cannot fail.
    Ok(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn uid() -> Result<u32> {
    bail!("module 'os' has no attribute 'getuid'")
}

fn gui_domain() -> Result<String> {
    Ok(format!("gui/{}", uid()?))
}

fn gui_target() -> Result<String> {
    Ok(format!("{}/{LABEL}", gui_domain()?))
}

/// `launchagent.plist_content`: the LaunchAgent plist XML.
fn plist_content(program: &str, log_dir: &Path) -> Result<String> {
    let string = |text: &str| Plist::String(text.to_string());
    let log = |name: &str| string(&log_dir.join(name).to_string_lossy());
    let plist = Plist::Dict(
        [
            ("Label", string(LABEL)),
            ("ProgramArguments", Plist::Array(vec![string(program)])),
            ("RunAtLoad", Plist::Bool(true)),
            (
                "KeepAlive",
                Plist::Dict([("SuccessfulExit".to_string(), Plist::Bool(false))].into()),
            ),
            ("LimitLoadToSessionType", string("Aqua")),
            ("StandardOutPath", log("agent.out.log")),
            ("StandardErrorPath", log("agent.err.log")),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect(),
    );
    plist_xml(&plist).map_err(|message| anyhow!(message))
}

struct Launchctl {
    success: bool,
    stdout: String,
    stderr: String,
}

/// `subprocess.run(["launchctl", *args], capture_output=True, text=True)`.
fn launchctl<S: AsRef<OsStr>>(args: &[S]) -> Result<Launchctl> {
    let output = Command::new("launchctl")
        .args(args)
        .stdin(Stdio::inherit())
        .output()
        .map_err(|error| PyOsError::new(error, Some(Path::new("launchctl"))))?;
    Ok(Launchctl {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `launchagent.install`: write the plist, trim the logs and (re)load it.
fn darwin_install(io: &mut Io<'_>) -> Result<PathBuf> {
    let program = python::sibling("localflow");
    let path = plist_path()?;
    let logs = log_dir()?;
    if let Some(parent) = path.parent() {
        mkdir_parents(parent)?;
    }
    let content = plist_content(&program.to_string_lossy(), &logs)?;
    write_text(&path, &content)?;
    mkdir_parents(&logs)?;
    for name in AGENT_LOGS {
        trim_in_place(&logs.join(name), io)?;
    }
    launchctl(&[OsStr::new("bootout"), OsStr::new(&gui_target()?)])?;
    let bootstrap = launchctl(&[
        OsStr::new("bootstrap"),
        OsStr::new(&gui_domain()?),
        path.as_os_str(),
    ])?;
    if !bootstrap.success {
        let fallback = launchctl(&[OsStr::new("load"), OsStr::new("-w"), path.as_os_str()])?;
        if !fallback.success {
            return Err(RuntimeError(format!(
                "launchctl bootstrap failed: {}; launchctl load fallback also failed: {}",
                py_strip(&bootstrap.stderr),
                py_strip(&fallback.stderr)
            ))
            .into());
        }
    }
    Ok(path)
}

fn darwin_uninstall() -> Result<()> {
    launchctl(&["bootout", &gui_target()?])?;
    let path = plist_path()?;
    if path.exists() {
        unlink(&path)?;
    }
    Ok(())
}

fn darwin_status() -> Result<String> {
    if !plist_path()?.exists() {
        return Ok("not installed".to_string());
    }
    let result = launchctl(&["print", &gui_target()?])?;
    Ok(status_text(result.success, &result.stdout))
}

/// `launchagent.status` from the `launchctl print` result.
fn status_text(success: bool, stdout: &str) -> String {
    if !success {
        format!("not loaded ({LABEL})")
    } else if stdout.contains("state = running") {
        format!("running ({LABEL})")
    } else {
        format!("loaded, not running ({LABEL})")
    }
}

/// `logfiles.trim_in_place(path, root=log_dir)`: cut a log over
/// `MAX_LOG_BYTES` down to its newest ~`KEEP_BYTES` without replacing the
/// file. Symlinks, hard-linked files and other users' files are skipped.
fn trim_in_place(path: &Path, io: &mut Io<'_>) -> Result<bool> {
    if path.is_symlink() {
        return Ok(false);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let Ok(mut file) = options.open(path) else {
        return Ok(false);
    };
    let Ok(info) = file.metadata() else {
        return Ok(false);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if info.nlink() != 1 || info.uid() != uid()? {
            return Ok(false);
        }
    }
    if !info.is_file() || info.len() <= MAX_LOG_BYTES {
        return Ok(false);
    }
    match trim_open_file(&mut file, info.len()) {
        Ok(()) => {}
        Err(error) => {
            // logging's last-resort handler: the bare message on stderr.
            writeln!(
                io.err,
                "could not trim {}: {}",
                path.to_string_lossy(),
                PyOsError::new(error, None)
            )?;
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_at(file: &mut fs::File, size: u64, offset: u64) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    file.seek(SeekFrom::Start(offset))?;
    Read::by_ref(file).take(size).read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn trim_open_file(file: &mut fs::File, size: u64) -> io::Result<()> {
    let mut start = size - KEEP_BYTES;
    // Start on a line boundary when one is near.
    let head = read_at(file, KEEP_BYTES.min(64 * 1024), start)?;
    if let Some(newline) = head.iter().position(|&b| b == b'\n') {
        if newline + 1 < head.len() {
            start += newline as u64 + 1;
        }
    }
    let mut written = 0;
    let mut offset = start;
    while offset < size {
        let chunk = read_at(file, CHUNK_BYTES.min(size - offset), offset)?;
        if chunk.is_empty() {
            break;
        }
        file.seek(SeekFrom::Start(written))?;
        file.write_all(&chunk)?;
        written += chunk.len() as u64;
        offset += chunk.len() as u64;
    }
    file.set_len(written)
}

// --- win32 ------------------------------------------------------------------

#[cfg(not(windows))]
mod win32 {
    use std::path::PathBuf;

    use anyhow::{bail, Result};

    /// `import winreg` outside Windows.
    fn no_winreg<T>() -> Result<T> {
        bail!("No module named 'winreg'")
    }

    pub(super) fn install() -> Result<PathBuf> {
        no_winreg()
    }

    pub(super) fn uninstall() -> Result<()> {
        no_winreg()
    }

    pub(super) fn status() -> Result<String> {
        no_winreg()
    }
}

#[cfg(windows)]
mod win32 {
    use std::io;
    use std::path::PathBuf;
    use std::ptr;

    use anyhow::Result;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_QWORD, REG_SAM_FLAGS, REG_SZ,
    };

    use crate::pyfs::PyOsError;
    use crate::python;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    fn check(code: u32) -> Result<(), PyOsError> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(PyOsError::new(
                io::Error::from_raw_os_error(code as i32),
                None,
            ))
        }
    }

    /// An open `HKCU\…\Run` key, closed on drop.
    struct RunKey(HKEY);

    impl RunKey {
        fn open(access: REG_SAM_FLAGS) -> Result<Self, PyOsError> {
            let mut key: HKEY = ptr::null_mut();
            let subkey = wide(RUN_KEY);
            // SAFETY: `subkey` is NUL-terminated and `key` is a valid out-pointer.
            check(unsafe {
                RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key)
            })?;
            Ok(RunKey(key))
        }
    }

    impl Drop for RunKey {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful RegOpenKeyExW.
            unsafe { RegCloseKey(self.0) };
        }
    }

    pub(super) fn install() -> Result<PathBuf> {
        let program = python::sibling("localflow.exe");
        let key = RunKey::open(KEY_SET_VALUE)?;
        let name = wide("localflow");
        let data = wide(&format!("\"{}\"", program.to_string_lossy()));
        // SAFETY: `name` is NUL-terminated and `data` spans the byte count given.
        check(unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        })?;
        Ok(program)
    }

    pub(super) fn uninstall() -> Result<()> {
        let result = RunKey::open(KEY_SET_VALUE).and_then(|key| {
            let name = wide("localflow");
            // SAFETY: `name` is NUL-terminated.
            check(unsafe { RegDeleteValueW(key.0, name.as_ptr()) })
        });
        match result {
            Err(error) if !error.is_not_found() => Err(error.into()),
            _ => Ok(()),
        }
    }

    pub(super) fn status() -> Result<String> {
        match query() {
            Ok(command) => Ok(format!("installed ({command})")),
            Err(error) if error.is_not_found() => Ok("not installed".to_string()),
            Err(error) => Err(error.into()),
        }
    }

    /// `winreg.QueryValueEx(key, "localflow")[0]` formatted with `str()`.
    fn query() -> Result<String, PyOsError> {
        let key = RunKey::open(KEY_READ)?;
        let name = wide("localflow");
        let mut kind = 0;
        let mut size = 0u32;
        // SAFETY: a null data pointer asks only for the type and size.
        check(unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        })?;
        let mut data = vec![0u8; size as usize];
        // SAFETY: `data` holds `size` writable bytes.
        check(unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut size,
            )
        })?;
        data.truncate(size as usize);
        Ok(match kind {
            REG_DWORD if data.len() >= 4 => {
                u32::from_le_bytes([data[0], data[1], data[2], data[3]]).to_string()
            }
            REG_QWORD if data.len() >= 8 => {
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&data[..8]);
                u64::from_le_bytes(bytes).to_string()
            }
            _ => {
                // REG_SZ and REG_EXPAND_SZ: UTF-16 up to the first NUL.
                let units: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .take_while(|&unit| unit != 0)
                    .collect();
                String::from_utf16_lossy(&units)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_escapes_like_python() {
        assert_eq!(desktop_exec("/usr/bin/localflow"), "\"/usr/bin/localflow\"");
        assert_eq!(
            desktop_exec(r#"/a b/$HOME/`x`/"q"\z"#),
            r#""/a b/\$HOME/\`x\`/\"q\"\\z""#
        );
    }

    #[test]
    fn plist_content_matches_plistlib() {
        let logs = Path::new("/Users/me/.localflow");
        let xml = plist_content("/v/bin/localflow & co", logs).unwrap();
        let expected = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
             \t<key>Label</key>\n\t<string>com.cortexrnd.localflow</string>\n\
             \t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/v/bin/localflow &amp; co</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
             \t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
             </dict>\n</plist>\n",
            logs.join("agent.err.log").display(),
            logs.join("agent.out.log").display(),
        );
        assert_eq!(xml, expected);
        assert!(plist_content("a\u{7}", logs).is_err());
    }

    #[test]
    fn status_text_reads_launchctl_print() {
        assert_eq!(
            status_text(false, ""),
            "not loaded (com.cortexrnd.localflow)"
        );
        assert_eq!(
            status_text(true, "x\n\tstate = running\n"),
            "running (com.cortexrnd.localflow)"
        );
        assert_eq!(
            status_text(true, "\tstate = waiting\n"),
            "loaded, not running (com.cortexrnd.localflow)"
        );
    }

    #[test]
    fn trim_keeps_the_newest_whole_lines() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("agent.out.log");
        let line = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde\n";
        let mut data = Vec::new();
        while (data.len() as u64) <= MAX_LOG_BYTES {
            data.extend_from_slice(line);
        }
        data.extend_from_slice(b"tail");
        fs::write(&path, &data).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        trim_open_file(&mut file, data.len() as u64).unwrap();
        drop(file);
        let trimmed = fs::read(&path).unwrap();
        let start = data.len() as u64 - KEEP_BYTES;
        let first = data[start as usize..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap();
        assert_eq!(trimmed, &data[start as usize + first + 1..]);
        assert!(trimmed.starts_with(line));
    }
}
