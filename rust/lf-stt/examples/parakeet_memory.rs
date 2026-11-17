mod support;

#[cfg(feature = "parakeet")]
fn materialize_files(
    root: &std::path::Path,
    files: &[(&str, std::path::PathBuf)],
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    files
        .iter()
        .map(|(name, source)| {
            let target = root.join(name);
            std::fs::copy(source, &target)?;
            Ok(target)
        })
        .collect()
}

#[cfg(feature = "parakeet")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use clap::Parser;
    use lf_stt::{engines::ParakeetEngine, SpeechEngine};
    use std::{path::PathBuf, time::Instant};
    #[derive(Parser)]
    struct Args {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "cpu")]
        provider: String,
        #[arg(long, default_value_t = 4)]
        threads: i32,
        #[arg(long)]
        fp32: bool,
    }
    let args = Args::parse();
    let repo = if args.fp32 {
        "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2"
    } else {
        lf_stt::model_storage::PARAKEET_ONNX_REPO
    };
    let files: &[&str] = if args.fp32 {
        &["encoder.onnx", "decoder.onnx", "joiner.onnx", "tokens.txt"]
    } else {
        lf_stt::model_storage::PARAKEET_ONNX_FILES
    };
    let paths = files
        .iter()
        .map(|file| lf_stt::downloads::model_file(repo, file))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let materialized = tempfile::tempdir()?;
    let paths = if args.fp32 {
        let weights = lf_stt::downloads::model_file(repo, "encoder.weights")?;
        let mut provisioned: Vec<_> = files.iter().copied().zip(paths).collect();
        provisioned.push(("encoder.weights", weights));
        let mut copied = materialize_files(materialized.path(), &provisioned)?;
        copied.truncate(4);
        copied
    } else {
        paths
    };
    let clip = support::read_clip(None)?;
    let mut footprints = serde_json::Map::new();
    let mut rss = |label: &str| -> anyhow::Result<usize> {
        let mut rss = memory_stats::memory_stats()
            .map(|memory| memory.physical_mem)
            .context("RSS unavailable")?;
        footprints.insert(format!("footprint_{label}"), serde_json::Value::Null);
        if let Some(probe) = std::env::var_os("LF_MEMORY_PROBE") {
            let measurement = std::process::Command::new(probe)
                .arg(std::process::id().to_string())
                .output()?;
            anyhow::ensure!(measurement.status.success(), "memory probe failed");
            let values: serde_json::Value = serde_json::from_slice(&measurement.stdout)?;
            rss = values["rss"].as_u64().context("probe RSS")? as usize;
            footprints.insert(format!("footprint_{label}"), values["footprint"].clone());
        }
        #[cfg(target_os = "macos")]
        {
            let map = std::process::Command::new("vmmap")
                .args(["--summary", &std::process::id().to_string()])
                .output()?;
            anyhow::ensure!(
                map.status.success(),
                "vmmap failed: {}",
                String::from_utf8_lossy(&map.stderr)
            );
            std::fs::write(
                args.output.with_extension(format!("{label}.vmmap.txt")),
                map.stdout,
            )?;
            if matches!(label, "unload" | "relief") {
                let heap = std::process::Command::new("heap")
                    .args(["-s", &std::process::id().to_string()])
                    .output()?;
                std::fs::write(
                    args.output.with_extension(format!("{label}.heap.txt")),
                    [heap.stdout, heap.stderr].concat(),
                )?;
            }
        }
        Ok(rss)
    };
    let mut engine = ParakeetEngine::with_model_files(&args.provider, paths, args.threads)?;
    let before = rss("before")?;
    let info = engine.load()?;
    let after_load = rss("load")?;
    let start = Instant::now();
    let transcript = engine.transcribe(&clip.pcm, None)?.text;
    anyhow::ensure!(!transcript.trim().is_empty(), "empty benchmark transcript");
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let after_transcribe = rss("transcribe")?;
    engine.unload();
    anyhow::ensure!(!engine.loaded(), "recognizer survived unload");
    let after_unload = rss("unload")?;
    drop(engine);
    let after_drop = rss("drop")?;
    let relieved = lf_stt::release_idle_memory();
    let after_relief = rss("relief")?;
    let mut result = serde_json::json!({"repo":repo,"threads":args.threads,"provider":info.provider,
        "rss_before":before,"rss_after_load":after_load,"rss_after_transcribe":after_transcribe,
        "rss_after_unload":after_unload,"rss_after_drop":after_drop,"rss_after_relief":after_relief,
        "pressure_relief_bytes":relieved,"load_ms":info.load_ms,"latency_ms":latency_ms,
        "transcript":transcript,"clip":clip.name,"clip_sample_rate":clip.original_sample_rate});
    result.as_object_mut().unwrap().extend(footprints);
    std::fs::write(args.output, serde_json::to_vec_pretty(&result)?)?;
    println!("{result}");
    Ok(())
}

#[cfg(not(feature = "parakeet"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Parakeet feature is required")
}

#[cfg(all(test, feature = "parakeet"))]
mod tests {
    #[test]
    fn external_weights_are_materialized_beside_regular_model_files() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let names = [
            "encoder.onnx",
            "decoder.onnx",
            "joiner.onnx",
            "tokens.txt",
            "encoder.weights",
        ];
        let files: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let blob = source.path().join(format!("blob-{index}"));
                std::fs::write(&blob, name).unwrap();
                (*name, blob)
            })
            .collect();
        let result = super::materialize_files(target.path(), &files).unwrap();
        assert_eq!(result.len(), 5);
        for (path, name) in result.iter().zip(names) {
            assert_eq!(path.file_name().unwrap(), name);
            assert!(!path.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(path).unwrap(), name);
        }
        assert!(super::materialize_files(
            target.path(),
            &[("missing", source.path().join("absent"))]
        )
        .is_err());
        assert!(super::materialize_files(target.path(), &[])
            .unwrap()
            .is_empty());
    }
}
