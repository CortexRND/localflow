#[cfg(any(feature = "whisper", feature = "parakeet"))]
#[path = "../examples/support/mod.rs"]
mod support;

#[cfg(any(feature = "whisper", feature = "parakeet"))]
fn exercise(mut engine: impl lf_stt::SpeechEngine, model: &str) {
    let _lease = lf_stt::stt_lease::record(engine.id(), model, None, true).unwrap();
    let clip = support::read_clip(None).unwrap();
    eprintln!("{} (source: {} Hz)", clip.name, clip.original_sample_rate);
    engine.download().unwrap();
    engine.load().unwrap();
    assert!(engine.loaded());
    assert!(engine.transcribe(&[], None).unwrap().text.is_empty());
    assert!(!engine
        .transcribe(&clip.pcm, Some("Speech recognition."))
        .unwrap()
        .text
        .trim()
        .is_empty());
    assert!(engine.transcribe(&[f32::NAN; 512], None).is_err());
    engine.unload();
    assert!(!engine.loaded());
    assert!(engine.transcribe(&clip.pcm, None).is_err());
}

/// Downloads models: `cargo test -p lf-stt whisper_real_model -- --ignored`.
#[cfg(feature = "whisper")]
#[test]
#[ignore]
fn whisper_real_model() {
    exercise(
        lf_stt::engines::WhisperEngine::new("base.en", Some("en".into()), "cpu").unwrap(),
        "base.en",
    );
}

/// Downloads models: `cargo test -p lf-stt parakeet_real_model -- --ignored`.
#[cfg(feature = "parakeet")]
#[test]
#[ignore]
fn parakeet_real_model() {
    exercise(lf_stt::engines::ParakeetEngine::new("cpu").unwrap(), "base");
}
