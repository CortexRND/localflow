mod support;

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
    let clip = support::read_clip(None)?;
    let rss = || {
        memory_stats::memory_stats()
            .map(|memory| memory.physical_mem)
            .context("RSS unavailable")
    };
    let mut engine = ParakeetEngine::with_model_files(&args.provider, paths, args.threads)?;
    let before = rss()?;
    let info = engine.load()?;
    let after_load = rss()?;
    let start = Instant::now();
    let transcript = engine.transcribe(&clip.pcm, None)?.text;
    anyhow::ensure!(!transcript.trim().is_empty(), "empty benchmark transcript");
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let after_transcribe = rss()?;
    engine.unload();
    let result = serde_json::json!({"repo":repo,"threads":args.threads,"provider":info.provider,
        "rss_before":before,"rss_after_load":after_load,"rss_after_transcribe":after_transcribe,
        "rss_after_unload":rss()?,"load_ms":info.load_ms,"latency_ms":latency_ms,
        "transcript":transcript,"clip":clip.name,"clip_sample_rate":clip.original_sample_rate});
    std::fs::write(args.output, serde_json::to_vec_pretty(&result)?)?;
    println!("{result}");
    Ok(())
}

#[cfg(not(feature = "parakeet"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Parakeet feature is required")
}
