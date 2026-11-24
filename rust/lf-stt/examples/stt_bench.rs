mod support;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use lf_core::config::Config;
use lf_stt::{factory, stt_lease, EngineSpec, LoadInfo};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Parser)]
#[command(about = "Download, load and benchmark native STT (downloads excluded from load timing)")]
struct Args {
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "whisper-cpp,parakeet-onnx"
    )]
    engines: Vec<String>,
    #[arg(long, default_value = "base.en")]
    model: String,
    /// auto/cpu; macOS additionally metal (Whisper) or coreml (Parakeet).
    #[arg(long, default_value = "auto")]
    provider: String,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    clip: Option<PathBuf>,
    /// JSON clip list: `{"clips": [{"id", "path"}]}` or `[{"id", "path"}]`.
    /// Each engine is loaded once and run over every clip.
    #[arg(long, conflicts_with = "clip")]
    manifest: Option<PathBuf>,
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
    /// Discarded warm-up transcriptions per clip before the timed runs.
    #[arg(long, default_value_t = 0)]
    warmup: u32,
    #[arg(long)]
    json: Option<PathBuf>,
    #[arg(long)]
    table: Option<PathBuf>,
}

#[derive(Serialize)]
struct Measurement {
    engine: String,
    model: String,
    requested_provider: String,
    effective_provider: String,
    download_ms: f64,
    load_ms: f64,
    rss_before_bytes: usize,
    rss_after_bytes: usize,
    rss_after_unload_bytes: usize,
    latencies_ms: Vec<f64>,
    first_ms: f64,
    median_ms: f64,
    p95_ms: f64,
    real_time_factor: f64,
    transcript: String,
}

#[derive(Debug, Deserialize, PartialEq)]
struct ManifestClip {
    id: String,
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestFile {
    Object { clips: Vec<ManifestClip> },
    Array(Vec<ManifestClip>),
}

#[derive(Serialize)]
struct ClipMeasurement {
    id: String,
    clip: String,
    audio_seconds: f64,
    original_sample_rate: u32,
    latencies_ms: Vec<f64>,
    first_ms: f64,
    median_ms: f64,
    p95_ms: f64,
    real_time_factor: f64,
    transcript: String,
}

#[derive(Serialize)]
struct EngineMeasurement {
    engine: String,
    model: String,
    requested_provider: String,
    effective_provider: String,
    download_ms: f64,
    load_ms: f64,
    rss_before_bytes: usize,
    rss_after_bytes: usize,
    rss_after_unload_bytes: usize,
    clips: Vec<ClipMeasurement>,
}

struct Timed {
    latencies_ms: Vec<f64>,
    median_ms: f64,
    p95_ms: f64,
    transcript: String,
}

struct Loaded {
    spec: EngineSpec,
    lease: stt_lease::LeaseGuard,
    model: String,
    download_ms: f64,
    rss_before_bytes: usize,
    rss_after_bytes: usize,
    load: LoadInfo,
}

fn stats(latencies: &[f64]) -> (f64, f64) {
    let mut sorted = latencies.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = (sorted[(n - 1) / 2] + sorted[n / 2]) / 2.0;
    (median, sorted[(n as f64 * 0.95).ceil() as usize - 1])
}

fn rss() -> Result<usize> {
    Ok(memory_stats::memory_stats()
        .context("could not read process RSS")?
        .physical_mem)
}

/// Parse a manifest; relative clip paths resolve against `base` (the manifest's folder).
fn parse_manifest(text: &str, base: &Path) -> Result<Vec<ManifestClip>> {
    let parsed: ManifestFile = serde_json::from_str(text).context("invalid manifest JSON")?;
    let mut clips = match parsed {
        ManifestFile::Object { clips } | ManifestFile::Array(clips) => clips,
    };
    ensure!(!clips.is_empty(), "manifest lists no clips");
    let mut seen = std::collections::HashSet::new();
    for clip in &mut clips {
        ensure!(!clip.id.trim().is_empty(), "manifest clip id is empty");
        if !seen.insert(clip.id.clone()) {
            bail!("duplicate manifest clip id {:?}", clip.id);
        }
        if clip.path.is_relative() {
            clip.path = base.join(&clip.path);
        }
    }
    Ok(clips)
}

fn load_engine(args: &Args, engine: &str) -> Result<Loaded> {
    let mut config = Config::default();
    config.fields.insert("stt_backend".into(), json!(engine));
    config.fields.insert("model_size".into(), json!(args.model));
    config
        .fields
        .insert("language".into(), json!(args.language));
    let lease = stt_lease::record(factory::canonical_backend(engine)?, &args.model, None, true)
        .context("could not protect benchmark model")?;
    let mut spec = factory::from_config(&config, Some(&args.provider))?;
    let download_ms = spec.engine.download()?;
    let rss_before_bytes = rss()?;
    let load = spec.engine.load()?;
    let rss_after_bytes = rss()?;
    let model = if engine.starts_with("parakeet") {
        lf_stt::model_storage::PARAKEET_ONNX_REPO.into()
    } else {
        args.model.clone()
    };
    Ok(Loaded {
        spec,
        lease,
        model,
        download_ms,
        rss_before_bytes,
        rss_after_bytes,
        load,
    })
}

/// Unload, drop engine and lease, then report process RSS.
fn unload_engine(loaded: Loaded) -> Result<usize> {
    let Loaded {
        mut spec, lease, ..
    } = loaded;
    spec.engine.unload();
    drop(spec);
    drop(lease);
    rss()
}

fn time_clip(spec: &mut EngineSpec, pcm: &[f32], args: &Args) -> Result<Timed> {
    for _ in 0..args.warmup {
        spec.engine.transcribe(pcm, args.prompt.as_deref())?;
    }
    let mut latencies_ms = Vec::new();
    let mut transcript = String::new();
    for _ in 0..args.runs {
        let start = Instant::now();
        let result = spec.engine.transcribe(pcm, args.prompt.as_deref())?;
        latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        ensure!(
            !result.text.trim().is_empty(),
            "{} returned an empty transcript",
            spec.backend
        );
        transcript = result.text;
    }
    let (median_ms, p95_ms) = stats(&latencies_ms);
    Ok(Timed {
        latencies_ms,
        median_ms,
        p95_ms,
        transcript,
    })
}

fn emit(args: &Args, report: &serde_json::Value, table: &str) -> Result<()> {
    let encoded = serde_json::to_string_pretty(report)?;
    if let Some(path) = &args.json {
        std::fs::write(path, &encoded)?;
    }
    if let Some(path) = &args.table {
        std::fs::write(path, table)?;
    }
    println!("{encoded}");
    eprintln!("{table}");
    Ok(())
}

fn run_manifest(args: &Args, manifest: &Path) -> Result<()> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("could not read manifest {}", manifest.display()))?;
    let entries = parse_manifest(&text, manifest.parent().unwrap_or(Path::new(".")))?;
    let mut clips = Vec::new();
    for entry in entries {
        let clip = support::read_clip(Some(&entry.path))
            .with_context(|| format!("could not read clip {}", entry.id))?;
        clips.push((entry.id, clip));
    }
    let mut results = Vec::new();
    for engine in &args.engines {
        let mut loaded = load_engine(args, engine)?;
        let mut measured = Vec::new();
        for (id, clip) in &clips {
            let seconds = clip.pcm.len() as f64 / 16000.0;
            let timed = time_clip(&mut loaded.spec, &clip.pcm, args)?;
            measured.push(ClipMeasurement {
                id: id.clone(),
                clip: clip.name.clone(),
                audio_seconds: seconds,
                original_sample_rate: clip.original_sample_rate,
                first_ms: timed.latencies_ms[0],
                latencies_ms: timed.latencies_ms,
                median_ms: timed.median_ms,
                p95_ms: timed.p95_ms,
                real_time_factor: timed.median_ms / (seconds * 1000.0),
                transcript: timed.transcript,
            });
        }
        let engine_name = loaded.spec.backend.clone();
        let model = loaded.model.clone();
        let effective_provider = loaded.load.provider.clone();
        let download_ms = loaded.download_ms + loaded.load.download_ms;
        let load_ms = loaded.load.load_ms;
        let (rss_before_bytes, rss_after_bytes) = (loaded.rss_before_bytes, loaded.rss_after_bytes);
        let rss_after_unload_bytes = unload_engine(loaded)?;
        results.push(EngineMeasurement {
            engine: engine_name,
            model,
            requested_provider: args.provider.clone(),
            effective_provider,
            download_ms,
            load_ms,
            rss_before_bytes,
            rss_after_bytes,
            rss_after_unload_bytes,
            clips: measured,
        });
    }
    let mut table = format!(
        "Manifest: `{}` ({} clips), {} runs after {} warm-up; load includes Parakeet's 1 s warm-up.\n\n",
        manifest.display(),
        clips.len(),
        args.runs,
        args.warmup
    );
    table.push_str("| Engine / model | Clip | Requested → effective provider | Audio s | First ms | Median ms | p95 ms | RTF |\n|---|---|---|---:|---:|---:|---:|---:|\n");
    for result in &results {
        for clip in &result.clips {
            table.push_str(&format!(
                "| {} / {} | {} | {} → {} | {:.3} | {:.1} | {:.1} | {:.1} | {:.3} |\n",
                result.engine,
                result.model,
                clip.id,
                result.requested_provider,
                result.effective_provider,
                clip.audio_seconds,
                clip.first_ms,
                clip.median_ms,
                clip.p95_ms,
                clip.real_time_factor
            ));
        }
    }
    table.push_str("\n| Engine / model | Download/cache ms | Load ms | RSS before → loaded → after unload MiB |\n|---|---:|---:|---:|\n");
    for result in &results {
        table.push_str(&format!(
            "| {} / {} | {:.1} | {:.1} | {:.1} → {:.1} → {:.1} |\n",
            result.engine,
            result.model,
            result.download_ms,
            result.load_ms,
            result.rss_before_bytes as f64 / 1048576.0,
            result.rss_after_bytes as f64 / 1048576.0,
            result.rss_after_unload_bytes as f64 / 1048576.0
        ));
    }
    table.push_str("\nRSS is process-wide (allocator caches may survive unload).\n");
    let report = json!({ "mode": "manifest", "runs": args.runs, "warmup": args.warmup, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "results": results });
    emit(args, &report, &table)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    if let Some(manifest) = args.manifest.clone() {
        return run_manifest(&args, &manifest);
    }
    let clip = support::read_clip(args.clip.as_deref())?;
    let seconds = clip.pcm.len() as f64 / 16000.0;
    let mut results = Vec::new();
    for engine in &args.engines {
        let mut loaded = load_engine(&args, engine)?;
        let timed = time_clip(&mut loaded.spec, &clip.pcm, &args)?;
        let engine_name = loaded.spec.backend.clone();
        let model = loaded.model.clone();
        let effective_provider = loaded.load.provider.clone();
        let download_ms = loaded.download_ms + loaded.load.download_ms;
        let load_ms = loaded.load.load_ms;
        let (rss_before_bytes, rss_after_bytes) = (loaded.rss_before_bytes, loaded.rss_after_bytes);
        let rss_after_unload_bytes = unload_engine(loaded)?;
        results.push(Measurement {
            engine: engine_name,
            model,
            requested_provider: args.provider.clone(),
            effective_provider,
            download_ms,
            load_ms,
            rss_before_bytes,
            rss_after_bytes,
            rss_after_unload_bytes,
            first_ms: timed.latencies_ms[0],
            latencies_ms: timed.latencies_ms,
            median_ms: timed.median_ms,
            p95_ms: timed.p95_ms,
            real_time_factor: timed.median_ms / (seconds * 1000.0),
            transcript: timed.transcript,
        });
    }
    let mut table = format!("Clip: `{}` ({seconds:.3} s, {} Hz → 16000 Hz), {} runs; load includes Parakeet's 1 s warm-up.\n\n", clip.name, clip.original_sample_rate, args.runs);
    table.push_str("| Engine / model | Requested → effective provider / verification | Download/cache ms | Load ms | RSS before → loaded MiB | First ms | Median ms | p95 ms | RTF |\n|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for result in &results {
        table.push_str(&format!("| {} / {} | {} → {} | {:.1} | {:.1} | {:.1} → {:.1} | {:.1} | {:.1} | {:.1} | {:.3} |\n", result.engine, result.model, result.requested_provider, result.effective_provider, result.download_ms, result.load_ms, result.rss_before_bytes as f64 / 1048576.0, result.rss_after_bytes as f64 / 1048576.0, result.first_ms, result.median_ms, result.p95_ms, result.real_time_factor));
    }
    for result in &results {
        table.push_str(&format!(
            "\n**{} transcript:** {}\n",
            result.engine, result.transcript
        ));
    }
    table.push_str("\nParakeet CoreML requests are unverified: sherpa's C API does not expose session provider assignment or silent native fallback. Explicit initialization failures report cpu (fallback). Even an enabled CoreML EP may place unsupported operators on CPU. RSS is process-wide (allocator caches may survive unload).\n");
    let report = json!({ "clip": clip.name, "fixed_clip": support::FIXED_CLIP, "audio_seconds": seconds, "original_sample_rate": clip.original_sample_rate, "sample_rate": 16000, "runs": args.runs, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "results": results });
    emit(&args, &report, &table)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn statistics_and_argument_validation() {
        assert_eq!(stats(&[4.0, 1.0, 2.0, 3.0]), (2.5, 4.0));
        assert_eq!(stats(&[1.0]), (1.0, 1.0));
        assert!(Args::try_parse_from(["bench", "--runs", "0"]).is_err());
        assert!(Args::try_parse_from(["bench", "--runs", "bad"]).is_err());
    }

    #[test]
    fn manifest_and_warmup_arguments() {
        let args = Args::try_parse_from(["bench"]).unwrap();
        assert!(args.manifest.is_none());
        assert_eq!(args.warmup, 0);
        let args =
            Args::try_parse_from(["bench", "--manifest", "m.json", "--warmup", "2"]).unwrap();
        assert_eq!(args.manifest, Some(PathBuf::from("m.json")));
        assert_eq!(args.warmup, 2);
        assert!(Args::try_parse_from(["bench", "--warmup", "-1"]).is_err());
        assert!(Args::try_parse_from(["bench", "--warmup", "x"]).is_err());
        assert!(
            Args::try_parse_from(["bench", "--manifest", "m.json", "--clip", "a.wav"]).is_err()
        );
    }

    #[test]
    fn manifest_object_and_array_forms() {
        let base = Path::new("/corpus");
        let object = r#"{"clips": [{"id": "a", "path": "a.wav"}, {"id": "b", "path": "/abs/b.wav", "reference": "ignored"}]}"#;
        let clips = parse_manifest(object, base).unwrap();
        assert_eq!(
            clips,
            vec![
                ManifestClip {
                    id: "a".into(),
                    path: PathBuf::from("/corpus/a.wav")
                },
                ManifestClip {
                    id: "b".into(),
                    path: PathBuf::from("/abs/b.wav")
                },
            ]
        );
        let array = r#"[{"id": "c", "path": "/x/c.wav"}]"#;
        let clips = parse_manifest(array, base).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].id, "c");
        assert_eq!(clips[0].path, PathBuf::from("/x/c.wav"));
        assert!(parse_manifest("[]", base).is_err());
        assert!(parse_manifest(r#"{"clips": []}"#, base).is_err());
        assert!(parse_manifest("not json", base).is_err());
        assert!(parse_manifest(r#"[{"id": "a"}]"#, base).is_err());
        assert!(parse_manifest(r#"[{"id": " ", "path": "a.wav"}]"#, base).is_err());
        assert!(parse_manifest(
            r#"[{"id": "a", "path": "1.wav"}, {"id": "a", "path": "2.wav"}]"#,
            base
        )
        .is_err());
    }
}
