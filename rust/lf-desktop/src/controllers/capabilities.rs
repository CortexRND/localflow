//! The three launch facts that decide what may run in this window.
//!
//! - `sandbox`: `--sandbox`. Every backend is an in-memory fake; nothing
//!   native (microphone, keyboard listener, engine, HTTP server, keyring)
//!   is ever started, whatever else was requested.
//! - `runtime_compiled`: the `runtime` cargo feature (forwards to
//!   `lf-ui/runtime`): speech-model inspection/download/delete, the setup
//!   trial and the in-window engine exist in this binary.
//! - `engine_requested`: `--engine`. Even then the engine only starts when
//!   the user presses Start on Home.
//!
//! Default launch is a front end only: no capture outside an explicit setup
//! microphone test, no global keys, no engine, no embedded server.

/// How to get a runtime-enabled build.
pub const REBUILD: &str =
    "cargo build --manifest-path rust/lf-desktop/Cargo.toml --release --features runtime";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub sandbox: bool,
    pub runtime_compiled: bool,
    pub engine_requested: bool,
}

impl Capabilities {
    pub fn new(sandbox: bool, engine_requested: bool) -> Self {
        Capabilities {
            sandbox,
            runtime_compiled: cfg!(feature = "runtime"),
            engine_requested,
        }
    }

    /// Engine controls are shown (Start/Stop on Home).
    pub fn engine_allowed(&self) -> bool {
        self.runtime_compiled && self.engine_requested && !self.sandbox
    }

    /// Native speech operations (inspect/download/delete/trial) are real.
    pub fn native_speech(&self) -> bool {
        self.runtime_compiled && !self.sandbox
    }

    /// The real microphone may be opened (only after an explicit click).
    pub fn real_microphone(&self) -> bool {
        !self.sandbox
    }

    /// One line explaining what this window does about dictation.
    pub fn engine_note(&self) -> &'static str {
        if self.sandbox {
            "Sandbox: no microphone, shortcut listener or engine is used; runtime states are simulated."
        } else if self.engine_allowed() {
            "This window can run dictation itself. Press Start to begin; nothing starts on its own."
        } else if self.runtime_compiled {
            "This window isn't handling dictation. Relaunch with `--engine` to run it here."
        } else {
            "This build can't run dictation itself; it shows the status of the LocalFlow runtime."
        }
    }

    /// Why native speech work isn't possible here, with the next step.
    pub fn native_speech_reason(&self) -> Option<String> {
        if self.sandbox || self.runtime_compiled {
            None
        } else {
            Some(format!(
                "This build has no speech engine or model downloader. Rebuild with `--features runtime` ({REBUILD}) or use `lf setup`."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_controls_need_compiled_requested_and_not_sandbox() {
        for sandbox in [false, true] {
            for compiled in [false, true] {
                for requested in [false, true] {
                    let caps = Capabilities {
                        sandbox,
                        runtime_compiled: compiled,
                        engine_requested: requested,
                    };
                    assert_eq!(
                        caps.engine_allowed(),
                        compiled && requested && !sandbox,
                        "{caps:?}"
                    );
                    if sandbox {
                        assert!(!caps.native_speech() && !caps.real_microphone());
                        assert!(caps.engine_note().contains("Sandbox"));
                    }
                    if !compiled && !sandbox {
                        assert!(caps
                            .native_speech_reason()
                            .unwrap()
                            .contains("--features runtime"));
                    }
                }
            }
        }
        assert_eq!(
            Capabilities::new(false, false).runtime_compiled,
            cfg!(feature = "runtime")
        );
    }
}
