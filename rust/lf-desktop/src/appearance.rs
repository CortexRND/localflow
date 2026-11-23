//! The appearance choice (System / Light / Dark) of the desktop window.
//!
//! Stored on its own, next to this app's window lock:
//! `<instance dir>/appearance.json` (real data: `<localflow dir>/desktop/`;
//! sandbox: inside the sandbox directory). It is never written to
//! `config.toml` and never goes through `Services::apply`, so dictation and
//! provider settings can't be affected by it. A missing, unreadable or
//! unknown file reads as System; the next save repairs it.

use std::io::Write as _;
use std::path::PathBuf;

use serde_json::Value;

use crate::controllers::instance::instance_dir;
use crate::data::DataRoot;

/// File name inside the instance directory.
pub const FILE_NAME: &str = "appearance.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppearanceMode {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// In the order the Settings control shows them.
    pub const ALL: [AppearanceMode; 3] = [
        AppearanceMode::System,
        AppearanceMode::Light,
        AppearanceMode::Dark,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AppearanceMode::System => "System",
            AppearanceMode::Light => "Light",
            AppearanceMode::Dark => "Dark",
        }
    }

    /// The stored value.
    pub fn key(self) -> &'static str {
        match self {
            AppearanceMode::System => "system",
            AppearanceMode::Light => "light",
            AppearanceMode::Dark => "dark",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.key() == key)
    }

    /// Position in [`Self::ALL`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
}

/// Where the choice is stored for `root`.
pub fn path(root: &DataRoot) -> PathBuf {
    instance_dir(root).join(FILE_NAME)
}

/// The stored choice; System when the file is missing, unreadable or holds
/// something unknown (never an error: the window just follows the OS).
pub fn load(root: &DataRoot) -> AppearanceMode {
    std::fs::read_to_string(path(root))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("mode")
                .and_then(Value::as_str)
                .and_then(AppearanceMode::from_key)
        })
        .unwrap_or_default()
}

/// Store the choice: written to a temporary sibling, then renamed over the
/// file, so a reader never sees a half-written file.
pub fn save(root: &DataRoot, mode: AppearanceMode) -> std::io::Result<()> {
    let target = path(root);
    let dir = target
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| instance_dir(root));
    std::fs::create_dir_all(&dir)?;
    let temp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let body = format!("{{\n  \"mode\": \"{}\"\n}}\n", mode.key());
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(body.as_bytes())?;
        file.sync_all()
    });
    let result = written.and_then(|()| std::fs::rename(&temp, &target));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ConfigFile;
    use serde_json::json;

    fn root() -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        (dir, root)
    }

    #[test]
    fn a_fresh_root_follows_the_system_and_every_choice_round_trips() {
        let (_dir, root) = root();
        assert!(!path(&root).exists());
        assert_eq!(load(&root), AppearanceMode::System);
        for mode in [
            AppearanceMode::Dark,
            AppearanceMode::Light,
            AppearanceMode::System,
        ] {
            save(&root, mode).unwrap();
            assert_eq!(load(&root), mode);
        }
        // No temporary file is left next to it.
        let names: Vec<_> = std::fs::read_dir(path(&root).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(FILE_NAME))
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()]);
    }

    #[test]
    fn garbage_or_unknown_values_read_as_system_and_a_save_repairs_them() {
        let (_dir, root) = root();
        let file = path(&root);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        for content in [
            "not json at all",
            "",
            "{\"mode\": \"sepia\"}",
            "{\"mode\": 3}",
            "[\"dark\"]",
            "{\"mode\": \"Dark\"}",
        ] {
            std::fs::write(&file, content).unwrap();
            assert_eq!(load(&root), AppearanceMode::System, "{content:?}");
        }
        save(&root, AppearanceMode::Dark).unwrap();
        assert_eq!(load(&root), AppearanceMode::Dark);
        save(&root, AppearanceMode::Light).unwrap();
        assert_eq!(load(&root), AppearanceMode::Light);
    }

    #[test]
    fn a_failed_save_returns_an_error_leaves_no_temp_file_and_reads_as_system() {
        let (_dir, root) = root();
        let file = path(&root);
        // A directory where the file belongs: the rename over it must fail.
        std::fs::create_dir_all(&file).unwrap();
        std::fs::write(file.join("keep"), "x").unwrap();

        assert!(save(&root, AppearanceMode::Dark).is_err());
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(FILE_NAME))
            .collect();
        assert_eq!(names, vec![FILE_NAME.to_string()], "no temp file is left");
        assert!(file.is_dir(), "the blocking directory is untouched");
        assert!(file.join("keep").is_file());
        assert_eq!(load(&root), AppearanceMode::System);
    }

    #[test]
    fn appearance_and_config_are_stored_independently() {
        let (_dir, root) = root();
        let config = ConfigFile::new(root.clone());
        config
            .apply(&json!({"dictation": {"enabled": true}}))
            .unwrap();
        let before = std::fs::read(&root.config_path).unwrap();

        save(&root, AppearanceMode::Dark).unwrap();
        assert_eq!(
            std::fs::read(&root.config_path).unwrap(),
            before,
            "saving the appearance leaves config.toml byte-identical"
        );
        assert_ne!(path(&root), root.config_path);

        let saved = config
            .apply(&json!({"dictation": {"enabled": false}}))
            .unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert_eq!(
            load(&root),
            AppearanceMode::Dark,
            "a config update leaves the stored appearance unchanged"
        );
        let text = std::fs::read_to_string(&root.config_path).unwrap();
        assert!(!text.contains("appearance"), "{text}");
    }

    #[test]
    fn a_sandbox_keeps_its_appearance_inside_the_sandbox() {
        let (dir, root) = root();
        let file = path(&root);
        assert!(file.starts_with(dir.path()), "{}", file.display());
        save(&root, AppearanceMode::Light).unwrap();
        assert!(file.is_file());
        assert!(file.starts_with(root.sandbox_dir.as_ref().unwrap()));
    }

    #[test]
    fn keys_and_indices_are_stable() {
        for (i, mode) in AppearanceMode::ALL.into_iter().enumerate() {
            assert_eq!(AppearanceMode::from_key(mode.key()), Some(mode));
            assert_eq!(mode.index(), i);
        }
        assert_eq!(AppearanceMode::default(), AppearanceMode::System);
    }
}
