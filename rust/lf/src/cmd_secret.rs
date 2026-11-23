//! The hidden `secret` group (localflow/cli.py `secret`, over
//! localflow/secrets.py): store, delete and report keyring secrets. Values
//! are never printed.

use anyhow::Result;
use lf_core::secrets::{self, SecretsUnavailable};

use crate::args::SecretCommand;
use crate::{fail, outln, prompt, CliError, Io};

/// The secrets `secret status` reports, in Python's order.
const STATUS_NAMES: [&str; 2] = ["llm_api_key", "fireworks_api_key"];

pub(crate) fn run(command: SecretCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        SecretCommand::Set { name } => {
            let Some(value) = prompt::hidden("secret", io)? else {
                outln!(io.err, "Aborted!")?;
                return Err(CliError::Exit(1).into());
            };
            secrets::set_secret(&name, &value).or_else(unavailable)?;
            outln!(io.out, "stored {name}")
        }
        SecretCommand::Unset { name } => {
            secrets::delete_secret(&name).or_else(unavailable)?;
            outln!(io.out, "removed {name}")
        }
        SecretCommand::Status => {
            for name in STATUS_NAMES {
                let state = if secrets::get_secret(name).is_empty() {
                    "not configured"
                } else {
                    "configured"
                };
                outln!(io.out, "{name}: {state}")?;
            }
            Ok(())
        }
    }
}

/// `raise click.ClickException(str(exc))` for a `SecretsUnavailable`.
fn unavailable(error: anyhow::Error) -> Result<()> {
    match error.downcast_ref::<SecretsUnavailable>() {
        Some(SecretsUnavailable(message)) => fail(message.clone()),
        None => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, MutexGuard};

    use lf_core::secrets::{set_keyring_backend, FailingKeyring, MemoryKeyring, SERVICE};

    use crate::testing::{run_cli, run_with, FakeClipboard, ENV_LOCK};

    fn keyring_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn set_and_unset_use_the_keyring() {
        let _guard = keyring_lock();
        let memory = Arc::new(MemoryKeyring::new());
        let previous = set_keyring_backend(Some(memory.clone()));
        let stored = run_with(
            &["secret", "set", "test_name"],
            "\nsk-value-1234\n",
            &mut FakeClipboard::default(),
        );
        assert_eq!(stored.code, 0, "{}", stored.err);
        assert_eq!(stored.out, "secret: \nsecret: \nstored test_name\n");
        assert_eq!(
            memory.value(SERVICE, "test_name").as_deref(),
            Some("sk-value-1234")
        );
        let aborted = run_with(
            &["secret", "set", "other"],
            "",
            &mut FakeClipboard::default(),
        );
        assert_eq!((aborted.code, aborted.err.as_str()), (1, "Aborted!\n"));
        assert_eq!(memory.value(SERVICE, "other"), None);
        let removed = run_cli(&["secret", "unset", "test_name"]);
        assert_eq!(
            (removed.code, removed.out.as_str()),
            (0, "removed test_name\n")
        );
        assert_eq!(memory.value(SERVICE, "test_name"), None);
        set_keyring_backend(previous);
    }

    #[test]
    fn keyring_failures_are_click_errors() {
        let _guard = keyring_lock();
        let previous = set_keyring_backend(Some(Arc::new(FailingKeyring("no keyring".into()))));
        let set = run_with(
            &["secret", "set", "x"],
            "sk-hidden\n",
            &mut FakeClipboard::default(),
        );
        assert_eq!((set.code, set.err.as_str()), (1, "Error: no keyring\n"));
        assert!(!set.out.contains("sk-hidden"));
        let unset = run_cli(&["secret", "unset", "x"]);
        assert_eq!((unset.code, unset.err.as_str()), (1, "Error: no keyring\n"));
        set_keyring_backend(previous);
    }
}
