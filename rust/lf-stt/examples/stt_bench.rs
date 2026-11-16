mod support;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use lf_core::config::Config;
use lf_stt::{factory, stt_lease};
use serde::Serialize;
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
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
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
    latencies_ms: Vec<f64>,
    first_ms: f64,
    median_ms: f64,
    p95_ms: f64,
    real_time_factor: f64,
    transcript: String,
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    let clip = support::read_clip(args.clip.as_deref())?;
    let seconds = clip.pcm.len() as f64 / 16000.0;
    let mut results = Vec::new();
    for engine in &args.engines {
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
        let mut latencies_ms = Vec::new();
        let mut transcript = String::new();
        for _ in 0..args.runs {
            let start = Instant::now();
            let result = spec.engine.transcribe(&clip.pcm, args.prompt.as_deref())?;
            latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            ensure!(
                !result.text.trim().is_empty(),
                "{} returned an empty transcript",
                spec.backend
            );
            transcript = result.text;
        }
        let (median_ms, p95_ms) = stats(&latencies_ms);
        results.push(Measurement {
            engine: spec.backend,
            model: if engine.starts_with("parakeet") {
                lf_stt::model_storage::PARAKEET_ONNX_REPO.into()
            } else {
                args.model.clone()
            },
            requested_provider: args.provider.clone(),
            effective_provider: load.provider,
            download_ms: download_ms + load.download_ms,
            load_ms: load.load_ms,
            rss_before_bytes,
            rss_after_bytes,
            first_ms: latencies_ms[0],
            latencies_ms,
            median_ms,
            p95_ms,
            real_time_factor: median_ms / (seconds * 1000.0),
            transcript,
        });
        spec.engine.unload();
        drop(spec.engine);
        drop(lease);
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
    let encoded = serde_json::to_string_pretty(&report)?;
    if let Some(path) = args.json {
        std::fs::write(path, &encoded)?;
    }
    if let Some(path) = args.table {
        std::fs::write(path, &table)?;
    }
    println!("{encoded}");
    eprintln!("{table}");
    Ok(())
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
}
