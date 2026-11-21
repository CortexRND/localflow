#![cfg(target_os = "windows")]

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use lf_input::listener::{self, Support};
use lf_input::{HubActions, InputError, KeyboardHub, Shortcuts};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F18, VK_F20,
    VK_LCONTROL,
};

fn send_key(vk: u16, down: bool) -> u32 {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
                ..Default::default()
            },
        },
    };
    // SAFETY: one initialized INPUT, with the required ABI size.
    unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) }
}

fn input(vk: u16, down: bool) {
    assert_eq!(
        send_key(vk, down),
        1,
        "SendInput({vk:#x}, down={down}) failed: {}",
        std::io::Error::last_os_error()
    );
}

struct ReleaseOnDrop(u16);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        send_key(self.0, false);
    }
}

struct Listener {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<(), InputError>>>,
}

impl Listener {
    fn stop(&mut self) -> thread::Result<Result<(), InputError>> {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.stop();
        }
    }
}

// The hook thread has no focused window. SendInput goes through the OS input
// stream, not KeyboardHub, so this also requires an interactive desktop.
#[test]
#[ignore = "injects system-wide keystrokes; run on an isolated interactive Windows desktop"]
fn win32_hook_delivers_push_to_talk_and_chords() {
    assert_eq!(listener::support(), Support::Windows);
    let (tx, rx) = mpsc::channel();
    let note = |event| {
        let tx = tx.clone();
        Arc::new(move || {
            let _ = tx.send(event);
        })
    };
    let (hub, warnings) = KeyboardHub::new(
        &Shortcuts {
            hold_key: "f18".into(),
            toggle_combo: "<ctrl>+<f20>".into(),
            ..Shortcuts::default()
        },
        HubActions {
            on_down: note("hold down"),
            on_up: note("hold up"),
            on_toggle: note("toggle"),
            ..HubActions::noop()
        },
    );
    assert!(warnings.is_empty());

    let stop = Arc::new(AtomicBool::new(false));
    let listener_stop = stop.clone();
    let ready = note("ready");
    let mut listener = Listener {
        stop,
        thread: Some(thread::spawn(move || {
            let result = listener::run_cancellable_with_ready(
                &hub,
                Arc::new(|_| {}),
                listener_stop,
                false,
                ready,
            );
            if let Err(error) = &result {
                eprintln!("{error}");
            }
            result
        })),
    };
    let expect = |event| {
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(event));
    };
    expect("ready");

    let _hold = ReleaseOnDrop(VK_F18);
    input(VK_F18, true);
    expect("hold down");
    input(VK_F18, true); // repeated key-down must not restart recording
    input(VK_F18, false);
    expect("hold up");

    {
        let _ctrl = ReleaseOnDrop(VK_LCONTROL);
        let _f20 = ReleaseOnDrop(VK_F20);
        input(VK_LCONTROL, true);
        input(VK_F20, true);
        expect("toggle");
    }

    input(VK_F18, true);
    expect("hold down");
    listener.stop().unwrap().unwrap();
    expect("hold up"); // stopping during a hold must not strand it
    assert!(rx.try_recv().is_err());
}
