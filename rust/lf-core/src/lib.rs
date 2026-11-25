//! LocalFlow core library (Stage 1 Rust port; the Python package remains the
//! shipped implementation and the behavioral specification).

pub mod apitoken;
pub mod config;
pub mod dictionary;
pub mod formatting;
pub mod history;
pub mod home;
pub mod hotphrases;
pub mod phrases_model;
pub mod secrets;
pub mod shortcuts;
pub mod symbols;
pub mod transcript;

/// Shared helpers for unit tests that touch process-global state.
#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::Mutex;

    /// Serializes tests that read or modify environment variables or the
    /// process-wide keyring backend.
    pub static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

    /// Sets/removes environment variables and restores them on drop. Hold
    /// [`GLOBAL_LOCK`] while it is alive.
    #[derive(Default)]
    pub struct EnvGuard {
        saved: Vec<(String, Option<OsString>)>,
    }

    impl EnvGuard {
        pub fn new() -> Self {
            Self::default()
        }

        fn remember(&mut self, name: &str) {
            if !self.saved.iter().any(|(saved, _)| saved == name) {
                self.saved.push((name.to_string(), std::env::var_os(name)));
            }
        }

        pub fn set(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) {
            self.remember(name);
            std::env::set_var(name, value);
        }

        pub fn remove(&mut self, name: &str) {
            self.remember(name);
            std::env::remove_var(name);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(&name, value),
                    None => std::env::remove_var(&name),
                }
            }
        }
    }
}
