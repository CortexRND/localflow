//! `~/.localflow/status.json` and `last.txt` (`localflow/status.py`).

use std::io::Write;
use std::path::{Path, PathBuf};

pub fn status_path() -> PathBuf {
    lf_core::home::localflow_dir().join("status.json")
}

pub fn last_text_path() -> PathBuf {
    lf_core::home::localflow_dir().join("last.txt")
}

pub fn title_prefix(state: &str) -> &'static str {
    match state {
        "recording" => "● ",
        "locked" => "🔒 ",
        "command" => "⌘ ",
        "processing" => "… ",
        _ => "",
    }
}

pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn write_state(state: &str, path: &Path) -> std::io::Result<()> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let body = serde_json::json!({"state": state, "since": since, "pid": std::process::id()});
    write_atomic(path, &body.to_string())
}

pub fn write_last_text(text: &str, path: &Path) -> std::io::Result<()> {
    write_atomic(path, text)
}

pub fn read_last_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_last_text_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("status.json");
        write_state("recording", &status).unwrap();
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&status).unwrap()).unwrap();
        assert_eq!(data["state"], "recording");
        assert_eq!(data["pid"], std::process::id());
        let last = dir.path().join("last.txt");
        assert_eq!(read_last_text(&last), "");
        write_last_text("hello", &last).unwrap();
        assert_eq!(read_last_text(&last), "hello");
        assert_eq!(title_prefix("locked"), "🔒 ");
        assert_eq!(title_prefix("idle"), "");
    }
}
