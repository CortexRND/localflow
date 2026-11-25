//! The shared speaker data format (sidecars, voiceprints, `.npy`, export ZIP,
//! transcript relabel) against `localflow/diarization` semantics. The
//! Python ↔ Rust round trip itself is `tests/test_rust_server.py`.

use lf_meetings::speakers::{
    bundle, npy,
    registry::{self, sweep, RegistryError},
    relabel::{relabel_text, relabel_transcript, vault_notes_path, RelabelError},
    store::{
        self, build_record, merge_speakers, problem, rename_speaker, speaker_lines, SpeakerEntry,
    },
    SpeakerData,
};
use lf_meetings::Segment;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path};

const ID: &str = "0123456789abcdef0123456789abcdef";

fn data(dir: &Path) -> SpeakerData {
    SpeakerData::with_roots(dir.join("meetings"), dir.join("speakers"))
}
fn segment(start: f64, source: &str, id: Option<&str>) -> Segment {
    Segment {
        offset_seconds: start,
        text: "never stored".into(),
        source: source.into(),
        end_seconds: Some(start + 1.0),
        speaker_id: id.map(Into::into),
        speaker_label: None,
    }
}
fn speaker(id: &str, label: &str, seconds: f64) -> SpeakerEntry {
    SpeakerEntry {
        id: id.into(),
        source: "others".into(),
        label: label.into(),
        name: None,
        voiceprint_id: None,
        seconds,
        centroid: None,
    }
}
fn record() -> Value {
    build_record(
        ID,
        "/v/log.md",
        "/v/notes.md",
        "2026-11-25T10:30:00",
        Some("mock"),
        "ok",
        &[
            speaker("S1", "Speaker 1", 2.25),
            speaker("S2", "Speaker 2", 1.0),
        ],
        &[
            segment(0.0, "me", None),
            segment(3.0, "others", Some("S1")),
            segment(3.5, "others", Some("S2")),
            segment(70.0, "others", Some("S1")),
            segment(80.0, "others", None),
        ],
    )
}

#[test]
fn record_shape_helpers_and_problems_match_store_py() {
    let r = record();
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "schema_version",
            "meeting_id",
            "log_path",
            "notes_path",
            "started_at",
            "embedder",
            "status",
            "speakers",
            "segments"
        ]
    );
    assert_eq!(
        r["segments"][0],
        json!({"start":0.0,"end":1.0,"source":"me","speaker_id":null})
    );
    assert_eq!(
        store::frontmatter_speakers(&r),
        ["Me", "Speaker 1", "Speaker 2"]
    );
    assert_eq!(
        speaker_lines(&r, "S1"),
        BTreeMap::from([("00:03".into(), vec![true]), ("01:10".into(), vec![true])])
    );
    // A label shared with another identity: flags select only this speaker's lines.
    let shared = rename_speaker(&r, "S2", "Speaker 1").unwrap();
    assert_eq!(
        speaker_lines(&shared, "S2"),
        BTreeMap::from([("00:03".into(), vec![false, true])])
    );
    let merged = merge_speakers(&r, "S2", "S1").unwrap();
    assert_eq!(
        merged["speakers"],
        json!([{"id":"S1","source":"others","label":"Speaker 1","name":null,"voiceprint_id":null,"seconds":3.2,"centroid":null}])
    );
    assert!(merge_speakers(&r, "S1", "S1").is_none());
    assert_eq!(
        store::public_speakers(&r)[0],
        json!({"id":"S1","label":"Speaker 1","name":null,"source":"others","seconds":2.25})
    );
    assert_eq!(problem(&r, ID), None);
    assert_eq!(problem(&r, &"f".repeat(32)), Some("shape"));
    for (path, value, reason) in [
        ("/schema_version", json!(2), "newer schema"),
        ("/schema_version", json!(true), "shape"),
        ("/schema_version", json!(1.0), "shape"),
        ("/schema_version", json!(0), "shape"),
        ("/log_path", json!(null), "shape"),
        ("/speakers/1/id", json!("S1"), "shape"),
        ("/speakers/0/seconds", json!(false), "shape"),
        ("/speakers/0/name", json!(3), "shape"),
        ("/speakers/0/centroid", json!([]), "shape"),
        ("/speakers/0/centroid", json!(["x"]), "shape"),
        ("/speakers/0/centroid", json!(vec![0.5; 4097]), "shape"),
        ("/segments/0/start", json!("0"), "shape"),
        ("/segments/0/speaker_id", json!(1), "shape"),
    ] {
        let mut bad = r.clone();
        *bad.pointer_mut(path).unwrap() = value;
        assert_eq!(problem(&bad, ID), Some(reason), "{path}");
    }
}

#[test]
fn sidecars_save_atomically_load_fail_closed_and_strip_voice_data() {
    let dir = tempfile::tempdir().unwrap();
    let data = data(dir.path());
    let store = data.store();
    assert!(store.load(ID).is_none());
    assert!(store.load("../escape").is_none());
    let mut r = record();
    r["speakers"][0]["centroid"] = json!([0.6, 0.8]);
    r["speakers"][0]["voiceprint_id"] = json!("a".repeat(32));
    r["future_key"] = json!({"kept": true});
    {
        let held = data.lock_sidecars();
        store.save(&held, &r).unwrap();
    }
    let path = store.path(ID).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\n  \"schema_version\": 1,\n") && text.ends_with("}\n"));
    assert_eq!(store.load(ID).unwrap(), r);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let names: Vec<_> = fs::read_dir(store.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names.len(), 1, "no staging file is left behind");

    // Unusable files read as absent; a newer version is never rewritten by load.
    let newer = store.root().join(format!("{}.json", "b".repeat(32)));
    fs::write(
        &newer,
        r#"{"schema_version": 9, "speakers": [{"centroid": [1.0], "voiceprint_id": "x"}], "x": 1}"#,
    )
    .unwrap();
    assert!(store.load(&"b".repeat(32)).is_none());
    fs::write(
        store.root().join(format!("{}.json", "c".repeat(32))),
        "not json",
    )
    .unwrap();
    let staging = store.root().join(format!(".{ID}.ab_c1234.tmp"));
    fs::write(&staging, "partial voice data").unwrap();
    let unrelated = store.root().join("notes.txt");
    fs::write(&unrelated, "keep").unwrap();
    #[cfg(unix)]
    let link = {
        let target = dir.path().join("target");
        fs::write(&target, "keep").unwrap();
        let link = store.root().join(format!(".{ID}.zzzzzzzz.tmp"));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        link
    };
    let (changed, failed) = {
        let held = data.lock_sidecars();
        store.strip_voiceprints(&held, None).unwrap()
    };
    assert_eq!((changed, failed), (2, 1), "corrupt file counts as failed");
    let stripped = store.load(ID).unwrap();
    assert!(stripped["speakers"][0]["centroid"].is_null());
    assert!(stripped["speakers"][0]["voiceprint_id"].is_null());
    assert_eq!(stripped["future_key"], json!({"kept": true}));
    let newer: Value = serde_json::from_slice(&fs::read(&newer).unwrap()).unwrap();
    assert_eq!(
        newer,
        json!({"schema_version": 9, "speakers": [{"centroid": null, "voiceprint_id": null}], "x": 1})
    );
    assert!(!staging.exists());
    assert!(unrelated.exists());
    #[cfg(unix)]
    assert!(
        link.symlink_metadata().is_ok(),
        "symlinks are not ours to delete"
    );
}

#[test]
fn npy_bytes_match_numpy_and_reading_is_narrow_and_fail_closed() {
    let bytes = npy::to_bytes(&[1.0, 0.0, 0.0, 0.0]);
    let header = "{'descr': '<f4', 'fortran_order': False, 'shape': (4,), }";
    assert_eq!(&bytes[..8], b"\x93NUMPY\x01\x00");
    assert_eq!((bytes.len() - 16) % 64, 0);
    assert_eq!(&bytes[10..10 + header.len()], header.as_bytes());
    assert_eq!(
        bytes[10 + header.len()..128 - 1]
            .iter()
            .filter(|&&b| b != b' ')
            .count(),
        0
    );
    assert_eq!(bytes[127], b'\n');
    assert_eq!(npy::from_bytes(&bytes).unwrap(), [1.0, 0.0, 0.0, 0.0]);
    let wide = npy::to_bytes(&vec![0.5; 192]);
    assert_eq!(npy::from_bytes(&wide).unwrap().len(), 192);
    // <f8 is read and re-normalised; anything else is unusable.
    let f8 = {
        let h = "{'descr': '<f8', 'fortran_order': False, 'shape': (2,), }";
        let mut h = h.to_string();
        while !(10 + h.len() + 1).is_multiple_of(64) {
            h.push(' ');
        }
        h.push('\n');
        let mut b = b"\x93NUMPY\x01\x00".to_vec();
        b.extend_from_slice(&(h.len() as u16).to_le_bytes());
        b.extend_from_slice(h.as_bytes());
        b.extend_from_slice(&3.0f64.to_le_bytes());
        b.extend_from_slice(&4.0f64.to_le_bytes());
        b
    };
    assert_eq!(npy::from_bytes(&f8).unwrap(), [0.6, 0.8]);
    for bad in [
        npy::to_bytes(&[0.0, 0.0]),
        npy::to_bytes(&[f32::NAN]),
        npy::to_bytes(&[]),
        String::from_utf8(npy::to_bytes(&[1.0]))
            .unwrap_or_default()
            .into_bytes(),
        npy::to_bytes(&[1.0, 2.0])[..129].to_vec(),
        b"not npy".to_vec(),
    ] {
        assert!(npy::from_bytes(&bad).is_none());
    }
    let mut ints = npy::to_bytes(&[1.0]);
    ints[20..23].copy_from_slice(b"<i4");
    assert!(npy::from_bytes(&ints).is_none());
}

#[test]
fn registry_enrolls_matches_renames_and_forgets_like_registry_py() {
    let dir = tempfile::tempdir().unwrap();
    let data = data(dir.path());
    let reg = data.registry();
    assert!(reg.list().is_empty());
    let enroll = |name: &str, v: Vec<f32>, model: &str, epoch| {
        let held = data.lock_registry();
        data.registry().enroll(&held, name, &[v], model, epoch)
    };
    assert!(matches!(
        enroll("!!", vec![1.0], "mock", None),
        Err(RegistryError::InvalidName)
    ));
    assert!(matches!(
        enroll("Ana", vec![1.0], "unknown-model", None),
        Err(RegistryError::UnsupportedModel)
    ));
    assert!(matches!(
        enroll("Ana", vec![0.0, 0.0], "mock", None),
        Err(RegistryError::BadVector(_))
    ));
    let epoch = data.forget_epoch();
    let ana = enroll(" Ana ", vec![3.0, 4.0], "mock", Some(epoch)).unwrap();
    assert_eq!(ana.name, "Ana");
    let vector =
        npy::from_bytes(&fs::read(reg.root().join(format!("{}.npy", ana.id))).unwrap()).unwrap();
    assert_eq!(vector, [0.6, 0.8]);
    let bob = enroll("bob", vec![0.8, 0.6], "mock", None).unwrap();
    let other = enroll(
        "ana",
        vec![0.6, 0.8],
        lf_meetings::diarize::models::EMBEDDING.id,
        None,
    )
    .unwrap();
    // Sorted by casefolded name, then id.
    let listed: Vec<String> = reg.list().iter().map(|v| v.name.to_lowercase()).collect();
    assert_eq!(listed, ["ana", "ana", "bob"]);
    assert!(matches!(reg.find("ANA"), Err(RegistryError::Ambiguous)));
    assert_eq!(reg.find(&bob.id).unwrap(), bob);
    assert!(matches!(reg.find("carol"), Err(RegistryError::Unknown)));
    // Same model and dimension only; threshold 0.9 for mock; ties: earlier enrollment.
    assert_eq!(reg.match_vector(&[0.6, 0.8], "mock").unwrap().0.id, ana.id);
    assert!(reg.match_vector(&[0.6, 0.8, 0.0], "mock").is_none());
    assert!(reg.match_vector(&[0.0, 1.0], "mock").is_none(), "0.8 < 0.9");
    assert_eq!(
        reg.match_vector(&[0.6, 0.8], lf_meetings::diarize::models::EMBEDDING.id)
            .unwrap()
            .0
            .id,
        other.id
    );
    assert!(reg.match_vector(&[0.6, 0.8], "nope").is_none());
    {
        let held = data.lock_registry();
        reg.rename(&held, &bob.id, "Robert").unwrap();
        assert!(matches!(
            reg.rename(&held, &"0".repeat(32), "X"),
            Err(RegistryError::Unknown)
        ));
    }
    assert_eq!(reg.find("robert").unwrap().id, bob.id);
    // A forget voids enrollments prepared before it.
    assert_eq!(reg.forget(Some(&bob.id)).unwrap(), 1);
    assert!(matches!(
        enroll("Late", vec![1.0], "mock", Some(epoch)),
        Err(RegistryError::Interrupted)
    ));
    assert!(!reg.root().join(format!("{}.npy", bob.id)).exists());
    let keep = reg.root().join("README.txt");
    fs::write(&keep, "mine").unwrap();
    fs::write(
        reg.root().join(format!(".{}.npy.x.tmp", "d".repeat(32))),
        "",
    )
    .unwrap();
    assert_eq!(reg.forget(None).unwrap(), 2);
    let left: Vec<_> = fs::read_dir(reg.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        left,
        ["README.txt"],
        "forget --all removes only owned files"
    );
    assert!(reg.list().is_empty());
    // Unreadable or newer registries are never overwritten.
    fs::write(
        reg.root().join("registry.json"),
        r#"{"schema_version": 2, "voiceprints": []}"#,
    )
    .unwrap();
    assert!(matches!(
        enroll("Ana", vec![1.0], "mock", None),
        Err(RegistryError::Unreadable)
    ));
}

#[test]
fn forget_all_ignores_foreign_unicode_names_in_the_private_folder() {
    let dir = tempfile::tempdir().unwrap();
    let data = data(dir.path());
    let root = data.registry().root().to_path_buf();
    fs::create_dir_all(&root).unwrap();
    // A two-byte character straddling byte 36 of the temp-name body, plus near misses.
    let foreign = [
        format!(".{}é.tmp", "a".repeat(35)),
        format!(".{}é.npy.x.tmp", "a".repeat(31)),
        format!(".{}.npy€.tmp", "0".repeat(32)),
        "日本語.npy".to_string(),
        ".registry.json".to_string(),
    ];
    for name in &foreign {
        fs::write(root.join(name), "not ours").unwrap();
    }
    let owned = root.join(format!(".{}.npy.ab12cd34.tmp", "0".repeat(32)));
    fs::write(&owned, "").unwrap();
    assert_eq!(data.registry().forget(None).unwrap(), 0);
    assert!(!owned.exists(), "owned temp files are still removed");
    for name in &foreign {
        assert!(root.join(name).exists(), "{name} is left alone");
    }
}

#[test]
fn staged_enrollment_audio_is_private_short_lived_and_swept() {
    let dir = tempfile::tempdir().unwrap();
    let data = data(dir.path());
    let reg = data.registry();
    let staged = reg
        .staged_audio(b"m4a bytes", Some(data.forget_epoch()))
        .unwrap();
    let path = staged.path().to_path_buf();
    assert!(registry::is_staged_name(
        &path.file_name().unwrap().to_string_lossy()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // forget --all sweeps an in-flight copy; finishing then tolerates it being gone.
    assert_eq!(reg.forget(None).unwrap(), 0);
    assert!(!path.exists());
    staged.finish().unwrap();
    assert!(matches!(
        reg.staged_audio(b"x", Some(0)),
        Err(RegistryError::Interrupted)
    ));
    let leftover = reg.root().join(format!(".enroll-{}.audio", "e".repeat(32)));
    fs::write(&leftover, "crash").unwrap();
    assert_eq!(sweep(reg.root()), (1, 0));
    assert_eq!(sweep(&dir.path().join("missing")), (0, 0));
    assert!(
        !dir.path().join("missing").exists(),
        "sweep creates nothing"
    );
    #[cfg(unix)]
    {
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let linked_data = SpeakerData::with_roots(dir.path().join("m"), &linked);
        assert!(matches!(
            linked_data.registry().staged_audio(b"x", None),
            Err(RegistryError::Other("could not prepare the audio"))
        ));
        assert!(fs::read_dir(&real).unwrap().next().is_none());
    }
}

#[test]
fn export_bundles_round_trip_and_hostile_archives_are_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let data = data(dir.path());
    let ana = {
        let held = data.lock_registry();
        data.registry()
            .enroll(&held, "Ana", &[vec![1.0, 0.0]], "mock", None)
            .unwrap()
    };
    let zip = data.registry().export_zip().unwrap();
    let target = dir.path().join("export");
    let (out, count) = bundle::write_bundle(&zip, &target).unwrap();
    assert_eq!((out, count), (target.clone(), 1));
    let mut names: Vec<_> = fs::read_dir(&target)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [format!("{}.npy", ana.id), "registry.json".into()]);
    assert!(matches!(
        bundle::write_bundle(&zip, &target),
        Err(RegistryError::Other(
            "the export folder must be new or empty"
        ))
    ));
    let refuse = |bytes: &[u8], message: &str| {
        let fresh = dir.path().join(format!("t{}", message.len()));
        match bundle::write_bundle(bytes, &fresh) {
            Err(RegistryError::Other(m)) => assert_eq!(m, message),
            other => panic!("{other:?}"),
        }
        assert!(!fresh.exists(), "nothing written for an invalid bundle");
    };
    refuse(b"not a zip", "not a speaker export");
    refuse(
        &vec![0; bundle::MAX_EXPORT_BYTES + 1],
        "not a speaker export",
    );
    let make = |members: &[(&str, &[u8])]| {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in members {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    };
    let npy_ok = npy::to_bytes(&[1.0]);
    let id = "f".repeat(32);
    let meta = |ids: &[&str]| {
        json!({"schema_version": 1, "voiceprints": ids.iter().map(|i| json!({"id": i, "name": "X", "created": "c", "samples": 1, "model_id": "mock"})).collect::<Vec<_>>()}).to_string()
    };
    let npy_name = format!("{id}.npy");
    refuse(
        &make(&[
            ("registry.json", meta(&[&id]).as_bytes()),
            ("../evil.npy", &npy_ok),
        ]),
        "the speaker export contains an unexpected file",
    );
    refuse(
        &make(&[
            ("registry.json", meta(&[&id]).as_bytes()),
            (&npy_name, b"junk"),
        ]),
        "the speaker export contains an unusable vector",
    );
    refuse(
        &make(&[("registry.json", meta(&[&id]).as_bytes())]),
        "the speaker export is incomplete",
    );
    refuse(
        &make(&[
            ("registry.json", meta(&[]).as_bytes()),
            (&npy_name, &npy_ok),
        ]),
        "the speaker export contains an unexpected file",
    );
    refuse(
        &make(&[(
            "registry.json",
            b"{\"schema_version\": 2, \"voiceprints\": []}",
        )]),
        "not a speaker export",
    );
    refuse(
        &make(&[(&npy_name, &vec![0u8; npy::MAX_FILE_BYTES + 1])]),
        "the speaker export contains an unexpected file",
    );
}

#[test]
fn relabel_rewrites_only_flagged_prefixes_and_the_speakers_entry() {
    let text = "---\ntitle: \"T\"\nmeeting_id: \"m1\"\nspeakers: [\"Me\", \"Speaker 1\"]\ntags: [meeting]\n---\n\n# T\n\n\
                **00:01** **Speaker 1:** hi\n\n**00:01** **Speaker 1:** again\n\n**00:02** **Me:** **Speaker 1:** quoted\n\n\
                ## My raw notes\n\n**00:01** **Speaker 1:** in notes\n";
    let lines = BTreeMap::from([("00:01".to_string(), vec![false, true])]);
    let speakers = vec!["Me".to_string(), "Dana".to_string()];
    let (out, count) = relabel_text(
        text,
        &lines,
        "Speaker 1",
        "Dana",
        Some(&speakers),
        Some("m1"),
    )
    .unwrap();
    assert_eq!(count, 1);
    assert!(out.contains("**00:01** **Speaker 1:** hi\n\n**00:01** **Dana:** again"));
    assert!(out.contains("**00:02** **Me:** **Speaker 1:** quoted"));
    assert!(out.ends_with("## My raw notes\n\n**00:01** **Speaker 1:** in notes\n"));
    assert!(out.contains("speakers: [\"Me\", \"Dana\"]\ntags"));
    assert!(matches!(
        relabel_text(text, &lines, "Speaker 1", "Dana", None, Some("other")),
        Err(RelabelError::Changed(
            "transcript does not belong to this meeting"
        ))
    ));
    let wrong = BTreeMap::from([("00:01".to_string(), vec![true])]);
    assert!(matches!(
        relabel_text(text, &wrong, "Speaker 1", "Dana", None, None),
        Err(RelabelError::Changed(
            "transcript lines do not match the speaker data"
        ))
    ));
    // Block-style lists (Obsidian Properties), comments, CRLF and quoted ids.
    let block = "---\r\nmeeting_id: 'm1'\r\nspeakers:\r\n  - Me\r\n\r\n  # c\r\n  - Speaker 1\r\ntags: x\r\n---\r\nbody\r\n";
    let (out, _) = relabel_text(
        block,
        &BTreeMap::new(),
        "a",
        "b",
        Some(&speakers),
        Some("m1"),
    )
    .unwrap();
    assert_eq!(
        out,
        "---\r\nmeeting_id: 'm1'\r\nspeakers: [\"Me\", \"Dana\"]\r\ntags: x\r\n---\r\nbody\r\n"
    );
    let (out, _) = relabel_text(
        "---\nmeeting_id: m1\nspeakers:\n  -bad\n---\n",
        &BTreeMap::new(),
        "a",
        "b",
        Some(&speakers),
        Some("m1"),
    )
    .unwrap();
    assert_eq!(
        out,
        "---\nmeeting_id: m1\nspeakers: [\"Me\", \"Dana\"]\n  -bad\n---\n"
    );
}

#[test]
fn relabel_transcript_keeps_mode_moves_mtime_two_seconds_and_resolves_inside_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    let log = vault.join("t (transcript).md");
    fs::write(
        &log,
        "---\nmeeting_id: \"m\"\n---\n**00:00** **Speaker 1:** hi\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&log, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mtime = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
    let start = mtime(&log);
    let lines = BTreeMap::from([("00:00".to_string(), vec![true])]);
    assert_eq!(
        relabel_transcript(&log, &lines, "Speaker 1", "Ana", None, Some("m")).unwrap(),
        1
    );
    let first = mtime(&log);
    let delta = |a: std::time::SystemTime, b: std::time::SystemTime| {
        a.duration_since(b)
            .or_else(|e| Ok::<_, ()>(e.duration()))
            .unwrap()
            .as_secs_f64()
    };
    assert!((delta(first, start) - 2.0).abs() < 0.01);
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "---\nmeeting_id: \"m\"\n---\n**00:00** **Ana:** hi\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    relabel_transcript(&log, &lines, "Ana", "Bo", None, Some("m")).unwrap();
    assert!(
        delta(mtime(&log), start) < 2.01,
        "alternates instead of drifting"
    );
    fs::write(&log, [0xff, 0xfe]).unwrap();
    assert!(matches!(
        relabel_transcript(&log, &lines, "Bo", "C", None, None),
        Err(RelabelError::NotReadable)
    ));
    assert!(matches!(
        relabel_transcript(&vault.join("nope.md"), &lines, "a", "b", None, None),
        Err(RelabelError::NotFound)
    ));
    assert_eq!(
        fs::read_dir(&vault).unwrap().count(),
        1,
        "no temp file left"
    );
    let vault_str = vault.to_string_lossy();
    assert_eq!(
        vault_notes_path(&log.to_string_lossy(), &vault_str),
        Some(fs::canonicalize(&log).unwrap())
    );
    let outside = dir.path().join("outside.md");
    fs::write(&outside, "x").unwrap();
    assert_eq!(
        vault_notes_path(&outside.to_string_lossy(), &vault_str),
        None
    );
    assert_eq!(
        vault_notes_path(
            &vault.join("../vault/../outside.md").to_string_lossy(),
            &vault_str
        ),
        None
    );
    let txt = vault.join("a.txt");
    fs::write(&txt, "x").unwrap();
    assert_eq!(vault_notes_path(&txt.to_string_lossy(), &vault_str), None);
}
