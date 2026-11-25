use lf_diarize::models::{model_path, DIAR_MODELS, SEGMENTATION};
use std::path::Path;

#[test]
fn catalog_matches_the_python_pins() {
    let python = include_str!("../../../localflow/diarization/models.py");
    for model in DIAR_MODELS {
        for value in [
            model.id,
            model.repo,
            model.revision,
            model.filename,
            model.sha256,
        ] {
            assert!(python.contains(&format!("\"{value}\"")), "{value}");
        }
        assert!(python.contains(&format!("size_bytes={},", model.size_bytes)));
    }
    assert_eq!(
        model_path(Path::new("/c"), &SEGMENTATION),
        Path::new("/c/models--csukuangfj--sherpa-onnx-pyannote-segmentation-3-0/snapshots/9403a6902bb58e3d5ae8c7e77c3422de279db2e0/model.onnx")
    );
}

#[cfg(feature = "sherpa")]
mod verify {
    use super::*;
    use lf_diarize::models::{verified_path, DiarModel, EMBEDDING};
    use std::fs;

    /// A model pinned to `bytes`, so the real hash check runs on a tiny file.
    fn pinned(bytes: &[u8]) -> DiarModel {
        use sha2::{Digest, Sha256};
        let hex: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        DiarModel {
            sha256: Box::leak(hex.into_boxed_str()),
            size_bytes: bytes.len() as u64,
            ..SEGMENTATION
        }
    }

    #[test]
    fn missing_copy_and_wrong_bytes_are_reported_and_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let model = pinned(b"weights");
        assert!(verified_path(dir.path(), &model)
            .unwrap_err()
            .contains("not downloaded"));
        let path = model_path(dir.path(), &model);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"weights").unwrap();
        assert_eq!(verified_path(dir.path(), &model).unwrap(), path);
        fs::write(&path, b"weightz").unwrap();
        assert!(verified_path(dir.path(), &model)
            .unwrap_err()
            .contains("checksum"));
        fs::write(&path, b"longer weights").unwrap();
        assert!(verified_path(dir.path(), &model)
            .unwrap_err()
            .contains("checksum"));
        assert!(path.is_file(), "verification never deletes");
        assert!(verified_path(dir.path(), &EMBEDDING).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hub_symlink_must_resolve_to_its_pinned_blob() {
        use lf_diarize::models::repo_folder;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let model = pinned(b"weights");
        let folder = repo_folder(dir.path(), &model);
        let path = model_path(dir.path(), &model);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir_all(folder.join("blobs")).unwrap();
        fs::write(folder.join("blobs").join(model.sha256), b"weights").unwrap();
        symlink(format!("../../blobs/{}", model.sha256), &path).unwrap();
        assert_eq!(verified_path(dir.path(), &model).unwrap(), path);
        // A link to an identical file elsewhere is not the pinned download.
        fs::write(dir.path().join("foreign"), b"weights").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(dir.path().join("foreign"), &path).unwrap();
        assert!(verified_path(dir.path(), &model)
            .unwrap_err()
            .contains("pinned download"));
        assert!(dir.path().join("foreign").is_file());
        // A linked revision folder is refused.
        let linked = tempfile::tempdir().unwrap();
        let other = pinned(b"x");
        let other_folder = repo_folder(linked.path(), &other);
        fs::create_dir_all(other_folder.join("snapshots")).unwrap();
        symlink(
            path.parent().unwrap(),
            other_folder.join("snapshots").join(other.revision),
        )
        .unwrap();
        assert!(verified_path(linked.path(), &other)
            .unwrap_err()
            .contains("plain folder"));
    }

    /// Runs only where `lf speakers download` already placed the real models.
    #[test]
    fn real_models_load_and_embed_when_present() {
        use lf_diarize::{sherpa::SherpaBackend, SpeakerBackend};
        let cache = std::env::var_os("HF_HUB_CACHE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HF_HOME").map(|h| std::path::PathBuf::from(h).join("hub"))
            })
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".cache/huggingface/hub"))
            });
        let Some(cache) = cache else { return };
        if DIAR_MODELS
            .iter()
            .any(|m| verified_path(&cache, m).is_err())
        {
            eprintln!("speaker models not downloaded; skipping");
            return;
        }
        let mut backend = SherpaBackend::new(cache);
        backend.load().unwrap();
        let tone: Vec<f32> = (0..32_000).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        // The pinned models were observed to find one turn of at least 1 s in
        // this tone; every turn must be well-formed, and the long ones embed
        // like the collector does.
        let seconds = tone.len() as f64 / 16_000.0;
        let whole = backend.embed(&tone).unwrap();
        assert!(!whole.is_empty() && whole.iter().all(|v| v.is_finite()));
        let turns = backend.diarize(&tone).unwrap();
        eprintln!("real turns on a {seconds}s tone: {turns:?}");
        assert!(!turns.is_empty(), "the pinned models found no turn");
        let mut embedded = 0;
        for turn in turns {
            // Clipped to the real audio: finite, non-empty, inside [0, duration].
            assert!(turn.start.is_finite() && turn.end.is_finite(), "{turn:?}");
            assert!(0.0 <= turn.start && turn.start < turn.end, "{turn:?}");
            assert!(turn.end <= seconds, "{turn:?}");
            let from = ((turn.start * 16_000.0) as usize).min(tone.len());
            let to = ((turn.end * 16_000.0) as usize).min(tone.len());
            if to.saturating_sub(from) >= 16_000 {
                let vector = backend.embed(&tone[from..to]).unwrap();
                assert_eq!(vector.len(), whole.len());
                assert!(vector.iter().all(|v| v.is_finite()));
                assert!(vector.iter().any(|&v| v != 0.0), "zero embedding");
                embedded += 1;
            }
        }
        assert!(embedded >= 1, "no turn of at least 1 s was embedded");
    }
}
