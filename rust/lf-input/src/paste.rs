//! Clipboard capture/restore and synthetic paste (port of `localflow/inject.py`
//! and the platform `paste`/`copy_selection` backends).
//!
//! Text is never logged: only method names and error kinds are.

use std::sync::Mutex;
use std::time::Duration;

/// How long the target app gets to read the clipboard before it is restored.
pub const PASTE_SETTLE: Duration = Duration::from_millis(300);
/// How long the target app gets to copy its selection.
pub const COPY_SETTLE: Duration = Duration::from_millis(150);
/// U+2063 marks the sentinel so it never collides with real clipboard text.
pub const SENTINEL_PREFIX: &str = "\u{2063}localflow-";

pub const PASTE_METHODS: &[&str] = &[
    "auto",
    "clipboard",
    "wtype",
    "xdotool",
    "pynput",
    "osascript",
];

pub trait Clipboard: Send {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str) -> bool;
}

pub trait Keystrokes: Send + Sync {
    /// Send the platform paste shortcut using `method`.
    fn paste(&self, method: &str) -> Result<(), String>;
    /// Send the platform copy shortcut.
    fn copy_selection(&self) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionCapture {
    pub text: String,
    pub clipboard_before: Option<String>,
}

pub struct Injector {
    clipboard: Mutex<Box<dyn Clipboard>>,
    keys: Box<dyn Keystrokes>,
    paste_settle: Duration,
    copy_settle: Duration,
}

impl Injector {
    pub fn new(clipboard: Box<dyn Clipboard>, keys: Box<dyn Keystrokes>) -> Self {
        Injector {
            clipboard: Mutex::new(clipboard),
            keys,
            paste_settle: PASTE_SETTLE,
            copy_settle: COPY_SETTLE,
        }
    }

    /// The OS clipboard and keystroke backend for this platform.
    pub fn native() -> Self {
        Injector::new(
            Box::new(SystemClipboard::default()),
            Box::new(NativeKeystrokes),
        )
    }

    pub fn with_settle(mut self, paste: Duration, copy: Duration) -> Self {
        self.paste_settle = paste;
        self.copy_settle = copy;
        self
    }

    fn clip(&self) -> std::sync::MutexGuard<'_, Box<dyn Clipboard>> {
        self.clipboard.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn clipboard_text(&self) -> Option<String> {
        self.clip().get()
    }

    /// Copy the current selection via a sentinel; restore the clipboard
    /// when nothing was selected.
    pub fn capture_selection(&self) -> SelectionCapture {
        let clipboard_before = self.clip().get();
        let sentinel = format!("{SENTINEL_PREFIX}{}", unique_suffix());
        if !self.clip().set(&sentinel) {
            return SelectionCapture {
                text: String::new(),
                clipboard_before,
            };
        }
        let copied = match self.keys.copy_selection() {
            Ok(()) => {
                std::thread::sleep(self.copy_settle);
                self.clip().get()
            }
            Err(_) => None,
        };
        match copied {
            Some(text) if text != sentinel => SelectionCapture {
                text,
                clipboard_before,
            },
            _ => {
                self.restore_clipboard_if_matches(&sentinel, clipboard_before.as_deref());
                SelectionCapture {
                    text: String::new(),
                    clipboard_before,
                }
            }
        }
    }

    /// Put `clipboard_before` back only if the clipboard still holds `expected`.
    pub fn restore_clipboard_if_matches(&self, expected: &str, clipboard_before: Option<&str>) {
        let Some(before) = clipboard_before else {
            return;
        };
        let mut clip = self.clip();
        if clip.get().as_deref() == Some(expected) {
            clip.set(before);
        }
    }

    /// Paste `text` into the focused app, then restore the previous clipboard
    /// unless something else replaced it meanwhile. Errors are swallowed,
    /// as in Python: a failed paste leaves the text on the clipboard.
    pub fn paste_text(&self, text: &str, method: &str, clipboard_before: Option<String>) {
        if method == "clipboard" {
            self.clip().set(text);
            return;
        }
        let old = clipboard_before.or_else(|| self.clip().get());
        if self.clip().set(text) {
            match self.keys.paste(method) {
                Ok(()) => std::thread::sleep(self.paste_settle),
                Err(error) => log::warn!("paste via {method} failed: {error}"),
            }
        }
        if let Some(old) = old {
            let mut clip = self.clip();
            if clip.get().as_deref() == Some(text) {
                clip.set(&old);
            }
        }
    }
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}{:x}", std::process::id())
}

/// The OS clipboard through `arboard`, kept alive so X11 ownership persists.
#[derive(Default)]
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
}

impl SystemClipboard {
    fn board(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.inner.is_none() {
            self.inner = arboard::Clipboard::new().ok();
        }
        self.inner.as_mut()
    }
}

impl Clipboard for SystemClipboard {
    fn get(&mut self) -> Option<String> {
        self.board()?.get_text().ok()
    }

    fn set(&mut self, text: &str) -> bool {
        self.board().is_some_and(|b| b.set_text(text).is_ok())
    }
}

pub struct NativeKeystrokes;

#[cfg(target_os = "macos")]
impl Keystrokes for NativeKeystrokes {
    fn paste(&self, method: &str) -> Result<(), String> {
        match method {
            // Pasteboard + Cmd+V as a CGEvent: no AppleScript round trip.
            "auto" => {
                if !crate::macos::accessibility_trusted() {
                    log::warn!("Accessibility is not granted; macOS may drop the synthetic Cmd+V");
                }
                crate::macos::post_command_key(9)
            }
            "osascript" => osascript_keystroke("v"),
            other => Err(format!("unknown paste method: {other:?}")),
        }
    }

    fn copy_selection(&self) -> Result<(), String> {
        crate::macos::post_command_key(8)
    }
}

#[cfg(target_os = "macos")]
fn osascript_keystroke(key: &str) -> Result<(), String> {
    let script =
        format!("tell application \"System Events\" to keystroke \"{key}\" using command down");
    run("osascript", &["-e", &script])
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Keystrokes for NativeKeystrokes {
    fn paste(&self, method: &str) -> Result<(), String> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let method = match method {
            "auto" if wayland && which("wtype") => "wtype",
            "auto" if which("xdotool") => "xdotool",
            "auto" => "pynput",
            other => other,
        };
        match method {
            "wtype" => run("wtype", &["-M", "ctrl", "v", "-m", "ctrl"]),
            "xdotool" => run("xdotool", &["key", "--clearmodifiers", "ctrl+v"]),
            "pynput" => Err(
                "direct key injection is not ported; install xdotool (X11) or wtype (Wayland)"
                    .into(),
            ),
            other => Err(format!("unknown paste method: {other:?}")),
        }
    }

    fn copy_selection(&self) -> Result<(), String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wtype") {
            run("wtype", &["-M", "ctrl", "c", "-m", "ctrl"])
        } else if which("xdotool") {
            run("xdotool", &["key", "--clearmodifiers", "ctrl+c"])
        } else {
            Err("copying the selection needs xdotool or wtype".into())
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let path = dir.join(program);
            std::fs::metadata(&path).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
    })
}

#[cfg(unix)]
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|e| format!("{program}: {e}"))
}

#[cfg(windows)]
impl Keystrokes for NativeKeystrokes {
    fn paste(&self, method: &str) -> Result<(), String> {
        match method {
            "auto" | "pynput" => windows_ctrl_chord(b'V'),
            other => Err(format!("unknown paste method: {other:?}")),
        }
    }

    fn copy_selection(&self) -> Result<(), String> {
        windows_ctrl_chord(b'C')
    }
}

#[cfg(windows)]
fn windows_ctrl_chord(letter: u8) -> Result<(), String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
    };
    let key = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [
        key(VK_CONTROL, false),
        key(u16::from(letter), false),
        key(u16::from(letter), true),
        key(VK_CONTROL, true),
    ];
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err("SendInput was blocked".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Board(Arc<Mutex<Option<String>>>);

    impl Clipboard for Board {
        fn get(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set(&mut self, text: &str) -> bool {
            *self.0.lock().unwrap() = Some(text.to_string());
            true
        }
    }

    /// Records keystrokes; optionally "copies" a selection or overwrites
    /// the clipboard like another app would.
    struct Keys {
        board: Board,
        log: Arc<Mutex<Vec<String>>>,
        selection: Option<String>,
        clobber: Option<String>,
    }

    impl Keystrokes for Keys {
        fn paste(&self, method: &str) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("paste:{method}"));
            if let Some(c) = &self.clobber {
                *self.board.0.lock().unwrap() = Some(c.clone());
            }
            if method == "bogus" {
                return Err("unknown".into());
            }
            Ok(())
        }
        fn copy_selection(&self) -> Result<(), String> {
            self.log.lock().unwrap().push("copy".into());
            if let Some(s) = &self.selection {
                *self.board.0.lock().unwrap() = Some(s.clone());
            }
            Ok(())
        }
    }

    fn injector(
        initial: Option<&str>,
        selection: Option<&str>,
        clobber: Option<&str>,
    ) -> (Injector, Board, Arc<Mutex<Vec<String>>>) {
        let board = Board::default();
        *board.0.lock().unwrap() = initial.map(str::to_string);
        let log = Arc::new(Mutex::new(Vec::new()));
        let keys = Keys {
            board: board.clone(),
            log: log.clone(),
            selection: selection.map(str::to_string),
            clobber: clobber.map(str::to_string),
        };
        let inj = Injector::new(Box::new(board.clone()), Box::new(keys))
            .with_settle(Duration::ZERO, Duration::ZERO);
        (inj, board, log)
    }

    #[test]
    fn paste_restores_previous_clipboard() {
        let (inj, board, log) = injector(Some("before"), None, None);
        inj.paste_text("hello", "auto", None);
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("before"));
        assert_eq!(*log.lock().unwrap(), vec!["paste:auto"]);
    }

    #[test]
    fn paste_keeps_clipboard_another_app_replaced() {
        let (inj, board, _) = injector(Some("before"), None, Some("theirs"));
        inj.paste_text("hello", "auto", None);
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("theirs"));
    }

    #[test]
    fn clipboard_method_only_copies() {
        let (inj, board, log) = injector(Some("before"), None, None);
        inj.paste_text("hello", "clipboard", None);
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("hello"));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_paste_still_restores() {
        let (inj, board, _) = injector(Some("before"), None, None);
        inj.paste_text("hello", "bogus", None);
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("before"));
    }

    #[test]
    fn explicit_clipboard_before_wins() {
        let (inj, board, _) = injector(Some("sentinel"), None, None);
        inj.paste_text("hello", "auto", Some("original".into()));
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("original"));
    }

    #[test]
    fn capture_selection_returns_copied_text() {
        let (inj, _, _) = injector(Some("before"), Some("selected"), None);
        let cap = inj.capture_selection();
        assert_eq!(cap.text, "selected");
        assert_eq!(cap.clipboard_before.as_deref(), Some("before"));
    }

    #[test]
    fn capture_without_selection_restores_clipboard() {
        let (inj, board, _) = injector(Some("before"), None, None);
        let cap = inj.capture_selection();
        assert_eq!(cap.text, "");
        assert_eq!(board.0.lock().unwrap().as_deref(), Some("before"));
    }
}
