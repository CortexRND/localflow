//! Port of `localflow/secrets.py`.
//!
//! Secrets live in the OS credential store under service [`SERVICE`] with the
//! secret name as the account, exactly where Python `keyring` keeps them, so
//! the Python and Rust implementations share stored keys:
//!
//! * macOS: login keychain generic password (service, account) — the
//!   `keyring` crate's `apple-native` store, like Python's macOS backend.
//! * Windows: Credential Manager generic credentials using Python
//!   `WinVaultKeyring`'s layout (target = service; a second account for the
//!   same service is kept under `account@service`), UTF-16 blobs.
//! * Linux/BSD: the Secret Service default collection, items searched by
//!   `{service, username}` and written with Python keyring's attributes and
//!   label so either implementation replaces the other's item.
//!
//! Tests (here and in other crates) install a [`MemoryKeyring`] with
//! [`set_keyring_backend`]; nothing touches a real credential store then.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;

const LOG_TARGET: &str = "localflow.secrets";

/// Keyring service name shared with the Python implementation.
pub const SERVICE: &str = "localflow";

/// Legacy environment variables honoured after `LOCALFLOW_<NAME>`.
const LEGACY_ENV: &[(&str, &str)] = &[
    ("fireworks_api_key", "FIREWORKS_API_KEY"),
    ("agent_devin_api_key", "DEVIN_API_KEY"),
];

static KEYRING_WARNED: AtomicBool = AtomicBool::new(false);

/// Raised (inside `anyhow::Error`) when the keyring cannot store or delete a
/// secret. Use `err.downcast_ref::<SecretsUnavailable>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsUnavailable(pub String);

impl fmt::Display for SecretsUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretsUnavailable {}

/// A credential store with Python keyring's `get/set/delete_password` contract:
/// `get` returns `None` for a missing entry, `delete` fails for a missing one.
pub trait KeyringBackend: Send + Sync {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>>;
    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()>;
    fn delete_password(&self, service: &str, name: &str) -> Result<()>;
}

static BACKEND_OVERRIDE: RwLock<Option<Arc<dyn KeyringBackend>>> = RwLock::new(None);

/// Replace the process-wide keyring backend (`None` restores the native OS
/// store) and return the previous override. Intended for tests.
pub fn set_keyring_backend(
    backend: Option<Arc<dyn KeyringBackend>>,
) -> Option<Arc<dyn KeyringBackend>> {
    let mut slot = BACKEND_OVERRIDE
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    std::mem::replace(&mut *slot, backend)
}

fn backend() -> Arc<dyn KeyringBackend> {
    let slot = BACKEND_OVERRIDE
        .read()
        .unwrap_or_else(|poison| poison.into_inner());
    match slot.as_ref() {
        Some(backend) => Arc::clone(backend),
        None => Arc::new(NativeKeyring),
    }
}

/// Reset the "warn once" latch for keyring failures (tests).
pub fn reset_keyring_warning() {
    KEYRING_WARNED.store(false, Ordering::SeqCst);
}

/// Log `keyring unavailable: ...` the first time only; true when it logged.
fn warn_keyring_once(err: &anyhow::Error) -> bool {
    if KEYRING_WARNED.swap(true, Ordering::SeqCst) {
        return false;
    }
    log::warn!(target: LOG_TARGET, "keyring unavailable: {err:#}");
    true
}

/// The environment variable that overrides secret `name`: `LOCALFLOW_<NAME>`.
pub fn env_name(name: &str) -> String {
    format!("LOCALFLOW_{}", name.to_uppercase())
}

/// Python `get_secret`: `LOCALFLOW_<NAME>` if present (even empty), then the
/// legacy variable (`FIREWORKS_API_KEY` for `fireworks_api_key` only), then
/// the keyring. Keyring failures log once and yield "".
pub fn get_secret(name: &str) -> String {
    if let Some(value) = std::env::var_os(env_name(name)) {
        return value.to_string_lossy().into_owned();
    }
    if let Some((_, legacy)) = LEGACY_ENV.iter().find(|(secret, _)| *secret == name) {
        if let Some(value) = std::env::var_os(legacy) {
            return value.to_string_lossy().into_owned();
        }
    }
    match backend().get_password(SERVICE, name) {
        Ok(value) => value.unwrap_or_default(),
        Err(err) => {
            warn_keyring_once(&err);
            String::new()
        }
    }
}

/// Python `set_secret`; any keyring failure is a [`SecretsUnavailable`].
pub fn set_secret(name: &str, value: &str) -> Result<()> {
    backend()
        .set_password(SERVICE, name, value)
        .map_err(unavailable)
}

/// Python `delete_secret`; any keyring failure (including a missing entry,
/// like keyring's `PasswordDeleteError`) is a [`SecretsUnavailable`].
pub fn delete_secret(name: &str) -> Result<()> {
    backend()
        .delete_password(SERVICE, name)
        .map_err(unavailable)
}

fn unavailable(err: anyhow::Error) -> anyhow::Error {
    warn_keyring_once(&err);
    let message = format!("{err:#}");
    anyhow::Error::new(SecretsUnavailable(message))
}

/// In-memory keyring for tests (mirrors the Python tests' `MemoryKeyring`:
/// deleting a missing entry is a no-op).
#[derive(Default)]
pub struct MemoryKeyring {
    values: Mutex<HashMap<(String, String), String>>,
}

impl MemoryKeyring {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populated with `(service, name) -> value` entries.
    pub fn with_values<'a>(
        values: impl IntoIterator<Item = ((&'a str, &'a str), &'a str)>,
    ) -> Self {
        let keyring = Self::new();
        {
            let mut map = keyring
                .values
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for ((service, name), value) in values {
                map.insert((service.to_string(), name.to_string()), value.to_string());
            }
        }
        keyring
    }

    /// The stored value for `(service, name)`.
    pub fn value(&self, service: &str, name: &str) -> Option<String> {
        let map = self
            .values
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.get(&(service.to_string(), name.to_string())).cloned()
    }

    /// Every `name -> value` stored for `service`.
    pub fn values(&self, service: &str) -> std::collections::BTreeMap<String, String> {
        let map = self
            .values
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.iter()
            .filter(|((stored, _), _)| stored == service)
            .map(|((_, name), value)| (name.clone(), value.clone()))
            .collect()
    }
}

impl KeyringBackend for MemoryKeyring {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>> {
        Ok(self.value(service, name))
    }

    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()> {
        let mut map = self
            .values
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.insert((service.to_string(), name.to_string()), value.to_string());
        Ok(())
    }

    fn delete_password(&self, service: &str, name: &str) -> Result<()> {
        let mut map = self
            .values
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.remove(&(service.to_string(), name.to_string()));
        Ok(())
    }
}

/// A keyring whose every operation fails (tests of the unavailable paths).
pub struct FailingKeyring(pub String);

impl KeyringBackend for FailingKeyring {
    fn get_password(&self, _service: &str, _name: &str) -> Result<Option<String>> {
        Err(anyhow::anyhow!("{}", self.0))
    }

    fn set_password(&self, _service: &str, _name: &str, _value: &str) -> Result<()> {
        Err(anyhow::anyhow!("{}", self.0))
    }

    fn delete_password(&self, _service: &str, _name: &str) -> Result<()> {
        Err(anyhow::anyhow!("{}", self.0))
    }
}

/// The OS credential store (see the module docs for each platform's layout).
pub struct NativeKeyring;

#[cfg(target_os = "macos")]
impl KeyringBackend for NativeKeyring {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>> {
        match keyring::Entry::new(service, name)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()> {
        Ok(keyring::Entry::new(service, name)?.set_password(value)?)
    }

    fn delete_password(&self, service: &str, name: &str) -> Result<()> {
        Ok(keyring::Entry::new(service, name)?.delete_credential()?)
    }
}

#[cfg(windows)]
mod windows_vault {
    //! Python keyring `WinVaultKeyring` semantics on top of `keyring`'s
    //! Windows generic-credential primitives.
    use anyhow::{anyhow, Result};
    use keyring::credential::CredentialApi as _;
    use keyring::windows::WinCredential;

    const COMMENT: &str = "Stored using python-keyring";

    fn credential(target: &str, service: &str, username: &str) -> Result<WinCredential> {
        let mut credential = WinCredential::new_with_target(Some(target), service, username)?;
        credential.comment = COMMENT.to_string();
        Ok(credential)
    }

    /// `DecodingCredential.value`: UTF-16 (BOM-aware, little-endian default),
    /// falling back to UTF-8.
    fn decode_blob(blob: &[u8]) -> Result<String> {
        if blob.len().is_multiple_of(2) {
            let (body, big_endian) = match blob {
                [0xFF, 0xFE, rest @ ..] => (rest, false),
                [0xFE, 0xFF, rest @ ..] => (rest, true),
                _ => (blob, false),
            };
            let units: Vec<u16> = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    if big_endian {
                        u16::from_be_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_le_bytes([pair[0], pair[1]])
                    }
                })
                .collect();
            if let Ok(text) = String::from_utf16(&units) {
                return Ok(text);
            }
        }
        let text = String::from_utf8(blob.to_vec())
            .map_err(|_| anyhow!("credential is neither UTF-16 nor UTF-8"))?;
        log::warn!(
            target: super::LOG_TARGET,
            "Retrieved a UTF-8 encoded credential. Please be aware that this library only \
             writes credentials in UTF-16."
        );
        Ok(text)
    }

    /// `(UserName, value)` stored under `target`, or None when not found.
    fn read(target: &str, service: &str) -> Result<Option<(String, String)>> {
        let probe = credential(target, service, "")?;
        let stored = match probe.get_credential() {
            Ok(stored) => stored,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let blob = match probe.get_secret() {
            Ok(blob) => blob,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some((stored.username, decode_blob(&blob)?)))
    }

    fn write(target: &str, service: &str, username: &str, password: &str) -> Result<()> {
        Ok(credential(target, service, username)?.set_password(password)?)
    }

    fn compound(username: &str, service: &str) -> String {
        format!("{username}@{service}")
    }

    pub fn get(service: &str, username: &str) -> Result<Option<String>> {
        let mut found = read(service, service)?;
        let matches =
            matches!(&found, Some((stored, _)) if username.is_empty() || stored == username);
        if !matches {
            found = read(&compound(username, service), service)?;
        }
        Ok(found.map(|(_, value)| value))
    }

    pub fn set(service: &str, username: &str, password: &str) -> Result<()> {
        if let Some((existing_user, existing_value)) = read(service, service)? {
            write(
                &compound(&existing_user, service),
                service,
                &existing_user,
                &existing_value,
            )?;
        }
        write(service, service, username, password)
    }

    pub fn delete(service: &str, username: &str) -> Result<()> {
        let mut deleted = false;
        for target in [service.to_string(), compound(username, service)] {
            if let Some((stored, _)) = read(&target, service)? {
                if stored == username {
                    deleted = true;
                    match credential(&target, service, username)?.delete_credential() {
                        Ok(()) | Err(keyring::Error::NoEntry) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }
        if !deleted {
            return Err(anyhow!("{service}"));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl KeyringBackend for NativeKeyring {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>> {
        windows_vault::get(service, name)
    }

    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()> {
        windows_vault::set(service, name, value)
    }

    fn delete_password(&self, service: &str, name: &str) -> Result<()> {
        windows_vault::delete(service, name)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod secret_service {
    //! Python keyring `SecretService.Keyring` semantics over D-Bus.
    use std::collections::HashMap;

    use anyhow::{anyhow, Result};
    use dbus_secret_service::{Collection, EncryptionType, Error, SecretService};

    /// Python keyring's `appid`; used so writes replace items either side made.
    const APPLICATION: &str = "Python keyring library";

    fn connect() -> Result<SecretService> {
        Ok(SecretService::connect(EncryptionType::Dh)?)
    }

    /// secretstorage `get_default_collection`, creating it when missing, then
    /// unlocked like `get_preferred_collection`.
    fn collection(service: &SecretService) -> Result<Collection<'_>> {
        let collection = match service.get_default_collection() {
            Ok(collection) => collection,
            Err(Error::NoResult) => service.create_collection("Default", "default")?,
            Err(err) => return Err(err.into()),
        };
        if collection.is_locked()? {
            collection
                .unlock()
                .map_err(|err| anyhow!("Failed to unlock the collection!: {err}"))?;
            if collection.is_locked()? {
                return Err(anyhow!("Failed to unlock the collection!"));
            }
        }
        Ok(collection)
    }

    fn query<'a>(service: &'a str, username: &'a str) -> HashMap<&'a str, &'a str> {
        HashMap::from([("service", service), ("username", username)])
    }

    pub fn get(service_name: &str, username: &str) -> Result<Option<String>> {
        let service = connect()?;
        let collection = collection(&service)?;
        let items = collection.search_items(query(service_name, username))?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        if item.is_locked()? {
            item.unlock()
                .map_err(|err| anyhow!("Failed to unlock the item!: {err}"))?;
            if item.is_locked()? {
                return Err(anyhow!("Failed to unlock the item!"));
            }
        }
        let secret = item.get_secret()?;
        Ok(Some(
            String::from_utf8(secret).map_err(|err| anyhow!("{err}"))?,
        ))
    }

    pub fn set(service_name: &str, username: &str, password: &str) -> Result<()> {
        let service = connect()?;
        let collection = collection(&service)?;
        let mut attributes = query(service_name, username);
        attributes.insert("application", APPLICATION);
        let label = format!("Password for '{username}' on '{service_name}'");
        collection.create_item(&label, attributes, password.as_bytes(), true, "text/plain")?;
        Ok(())
    }

    pub fn delete(service_name: &str, username: &str) -> Result<()> {
        let service = connect()?;
        let collection = collection(&service)?;
        let items = collection.search_items(query(service_name, username))?;
        match items.first() {
            Some(item) => Ok(item.delete()?),
            None => Err(anyhow!("No such password!")),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl KeyringBackend for NativeKeyring {
    fn get_password(&self, service: &str, name: &str) -> Result<Option<String>> {
        secret_service::get(service, name)
    }

    fn set_password(&self, service: &str, name: &str, value: &str) -> Result<()> {
        secret_service::set(service, name, value)
    }

    fn delete_password(&self, service: &str, name: &str) -> Result<()> {
        secret_service::delete(service, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvGuard, GLOBAL_LOCK};

    struct BackendGuard(Option<Arc<dyn KeyringBackend>>);

    impl BackendGuard {
        fn install(backend: Arc<dyn KeyringBackend>) -> Self {
            BackendGuard(set_keyring_backend(Some(backend)))
        }
    }

    impl Drop for BackendGuard {
        fn drop(&mut self) {
            set_keyring_backend(self.0.take());
        }
    }

    #[test]
    fn environment_takes_precedence_over_legacy_and_keyring() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(MemoryKeyring::with_values([(
            (SERVICE, "fireworks_api_key"),
            "keyring",
        )])));
        let mut env = EnvGuard::new();
        env.set("FIREWORKS_API_KEY", "legacy");
        env.set("LOCALFLOW_FIREWORKS_API_KEY", "local");
        assert_eq!(get_secret("fireworks_api_key"), "local");
        env.remove("LOCALFLOW_FIREWORKS_API_KEY");
        assert_eq!(get_secret("fireworks_api_key"), "legacy");
        env.remove("FIREWORKS_API_KEY");
        assert_eq!(get_secret("fireworks_api_key"), "keyring");
    }

    #[test]
    fn empty_environment_value_still_wins() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(MemoryKeyring::with_values([(
            (SERVICE, "llm_api_key"),
            "keyring",
        )])));
        let mut env = EnvGuard::new();
        env.set("LOCALFLOW_LLM_API_KEY", "");
        assert_eq!(get_secret("llm_api_key"), "");
    }

    #[test]
    fn legacy_variable_only_applies_to_fireworks_key() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(MemoryKeyring::with_values([(
            (SERVICE, "llm_api_key"),
            "keyring",
        )])));
        let mut env = EnvGuard::new();
        env.remove("LOCALFLOW_LLM_API_KEY");
        env.set("FIREWORKS_API_KEY", "legacy");
        env.set("LLM_API_KEY", "not-a-localflow-variable");
        assert_eq!(get_secret("llm_api_key"), "keyring");
    }

    #[test]
    fn keyring_fallback_and_missing_entry() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let memory = Arc::new(MemoryKeyring::with_values([(
            (SERVICE, "llm_api_key"),
            "keyring",
        )]));
        let _keyring = BackendGuard::install(memory.clone());
        let mut env = EnvGuard::new();
        env.remove("LOCALFLOW_LLM_API_KEY");
        env.remove("LOCALFLOW_OTHER");
        assert_eq!(get_secret("llm_api_key"), "keyring");
        assert_eq!(get_secret("other"), "");

        set_secret("other", "value").unwrap();
        assert_eq!(memory.value(SERVICE, "other").as_deref(), Some("value"));
        assert_eq!(get_secret("other"), "value");
        delete_secret("other").unwrap();
        assert_eq!(memory.value(SERVICE, "other"), None);
    }

    #[test]
    fn unavailable_keyring_reads_empty_and_raises_once_warned() {
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _keyring = BackendGuard::install(Arc::new(FailingKeyring("no backend".into())));
        let mut env = EnvGuard::new();
        env.remove("LOCALFLOW_LLM_API_KEY");
        reset_keyring_warning();

        assert_eq!(get_secret("llm_api_key"), "");
        assert!(KEYRING_WARNED.load(Ordering::SeqCst));
        let err = set_secret("llm_api_key", "value").unwrap_err();
        assert_eq!(
            err.downcast_ref::<SecretsUnavailable>().unwrap().0,
            "no backend"
        );
        let err = delete_secret("llm_api_key").unwrap_err();
        assert!(err.downcast_ref::<SecretsUnavailable>().is_some());
        // Only the first failure logs.
        assert!(!warn_keyring_once(&anyhow::anyhow!("again")));
        reset_keyring_warning();
    }

    #[test]
    fn env_name_uppercases() {
        assert_eq!(env_name("fireworks_api_key"), "LOCALFLOW_FIREWORKS_API_KEY");
        assert_eq!(env_name(""), "LOCALFLOW_");
    }
}
