//! The parent terminal's console for Windows release builds.
//!
//! Release builds on Windows are GUI-subsystem programs, so a terminal does
//! not give them a console. When no output handles were inherited (an
//! unredirected launch from `cmd` or PowerShell), [`attach_parent`] borrows
//! the parent's console for the command-line output and errors, and
//! [`detach`] leaves it once the window is open, so closing that terminal does
//! not end the app. Elsewhere both are no-ops.

#[cfg(all(windows, not(debug_assertions)))]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    static ATTACHED: AtomicBool = AtomicBool::new(false);

    pub fn attach_parent() {
        let missing = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .any(|which| {
                // SAFETY: GetStdHandle only reads this process's standard handles.
                let handle = unsafe { GetStdHandle(which) };
                handle.is_null() || handle == INVALID_HANDLE_VALUE
            });
        // SAFETY: a GUI-subsystem process starts without a console; a failed
        // attach (no parent console) leaves the process unchanged.
        if missing && unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0 {
            ATTACHED.store(true, Ordering::SeqCst);
        }
    }

    pub fn detach() {
        if ATTACHED.swap(false, Ordering::SeqCst) {
            // SAFETY: frees only the console this process attached to.
            unsafe { FreeConsole() };
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
mod imp {
    pub fn attach_parent() {}

    pub fn detach() {}
}

pub use imp::{attach_parent, detach};
