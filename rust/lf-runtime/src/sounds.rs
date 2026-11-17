//! Python-compatible native cues. The controller gates these on sounds_enabled.

#[cfg(any(target_os = "macos", test))]
fn play_with(cue: &str, mut run: impl FnMut(&str) -> std::io::Result<()>) -> std::io::Result<()> {
    let path = match cue {
        "start" => "/System/Library/Sounds/Pop.aiff",
        "stop" => "/System/Library/Sounds/Bottle.aiff",
        "lock" => "/System/Library/Sounds/Tink.aiff",
        "cancel" => "/System/Library/Sounds/Funk.aiff",
        "warn" => "/System/Library/Sounds/Sosumi.aiff",
        "error" => "/System/Library/Sounds/Basso.aiff",
        "achievement" => "/System/Library/Sounds/Glass.aiff",
        _ => return Ok(()),
    };
    run(path)
}

pub fn play(cue: &str) {
    #[cfg(target_os = "macos")]
    {
        let cue = cue.to_owned();
        let _ = std::thread::Builder::new()
            .name("localflow-sound".into())
            .spawn(move || {
                let _ = play_with(&cue, |path| {
                    std::process::Command::new("/usr/bin/afplay")
                        .arg(path)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map(|_| ())
                });
            });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cue;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cues_match_python_and_unknown_cues_do_nothing() {
        let mut paths = Vec::new();
        for cue in [
            "start",
            "stop",
            "lock",
            "cancel",
            "warn",
            "error",
            "achievement",
            "",
            "unknown",
        ] {
            play_with(cue, |path| {
                paths.push(path.to_owned());
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            paths,
            ["Pop", "Bottle", "Tink", "Funk", "Sosumi", "Basso", "Glass"]
                .map(|name| format!("/System/Library/Sounds/{name}.aiff"))
        );
        assert!(play_with("start", |_| Err(std::io::Error::other("audio unavailable"))).is_err());
        assert!(play_with("unknown", |_| panic!("must not run")).is_ok());
    }
}
