//! `setup install-app`: the Python `lf setup install-app` (`cli.py`) over
//! `localflow/appbundle.py` `install_app`, which writes the `LocalFlow
//! Setup.app` and `LocalFlow.app` launcher bundles into `~/Applications`.

#[cfg(any(unix, test))]
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use lf_core::home::expand_user;

use crate::args::SetupCommand;
use crate::pyfmt::{plist_xml, shlex_quote, Plist};
use crate::pyfs::{mkdir_parents, write_bytes, write_text, PyOsError};
use crate::richfmt::Console;
use crate::{fail, native, python, Io};

/// One bundle `install_app` writes.
struct Bundle<'a> {
    name: &'a str,
    bundle_id: &'a str,
    module: &'a str,
    /// `icon=True, full_plist=True`: version keys, `LSUIElement` and an icon.
    full: bool,
}

const BUNDLES: [Bundle<'static>; 2] = [
    Bundle {
        name: "LocalFlow Setup.app",
        bundle_id: "com.cortexrnd.localflow.setup",
        module: "localflow.setup_window",
        full: false,
    },
    Bundle {
        name: "LocalFlow.app",
        bundle_id: "com.cortexrnd.localflow.app",
        module: "localflow.main_app",
        full: true,
    },
];

pub(crate) fn run(command: SetupCommand, io: &mut Io<'_>) -> Result<()> {
    match command {
        SetupCommand::InstallApp => {
            let console = Console::detect();
            // `install_app`'s default `dest_dir`.
            let dest = expand_user(&Path::new("~").join("Applications"))?;
            // A native install's signed LocalFlow.app must stay untouched,
            // also when a Python install exists.
            if native::is_native_app(&dest.join("LocalFlow.app")) {
                return fail(native::NATIVE_APP);
            }
            // `sys.executable` of the Python LocalFlow install.
            let Some(python) = python::find_interpreter() else {
                return fail(python::NO_INSTALL);
            };
            for bundle in &BUNDLES {
                let path = install_app(&dest, &python.to_string_lossy(), bundle)?;
                write!(
                    io.out,
                    "{}",
                    console.markup(&format!(
                        "[green]installed[/green] {}",
                        path.to_string_lossy()
                    ))
                )?;
            }
            Ok(())
        }
    }
}

/// `appbundle.install_app`: (re)write the bundle and return its path.
fn install_app(dest: &Path, python: &str, bundle: &Bundle<'_>) -> Result<PathBuf> {
    let display_name = bundle.name.strip_suffix(".app").unwrap_or(bundle.name);
    let path = dest.join(bundle.name);
    let contents = path.join("Contents");
    let macos = contents.join("MacOS");
    mkdir_parents(&macos)?;

    let string = |text: &str| Plist::String(text.to_string());
    let mut plist = std::collections::BTreeMap::from([
        ("CFBundleName".to_string(), string(display_name)),
        ("CFBundleDisplayName".to_string(), string(display_name)),
        ("CFBundleIdentifier".to_string(), string(bundle.bundle_id)),
        ("CFBundleExecutable".to_string(), string("launcher")),
        ("CFBundlePackageType".to_string(), string("APPL")),
        ("CFBundleShortVersionString".to_string(), string("0.2.4")),
        ("NSHighResolutionCapable".to_string(), Plist::Bool(true)),
    ]);
    if bundle.full {
        let version = env!("CARGO_PKG_VERSION");
        plist.insert("CFBundleVersion".to_string(), string(version));
        plist.insert("CFBundleShortVersionString".to_string(), string(version));
        plist.insert("LSUIElement".to_string(), Plist::Bool(false));
        let resources = contents.join("Resources");
        mkdir_parents(&resources)?;
        if make_icns(&resources.join("LocalFlow.icns")) {
            plist.insert("CFBundleIconFile".to_string(), string("LocalFlow"));
        }
    }
    let xml = plist_xml(&Plist::Dict(plist)).map_err(|message| anyhow!(message))?;
    write_bytes(&contents.join("Info.plist"), xml.as_bytes())?;

    let launcher = macos.join("launcher");
    write_text(
        &launcher,
        &format!(
            "#!/bin/bash\nexec {} -m {} \"$@\"\n",
            shlex_quote(python),
            shlex_quote(bundle.module)
        ),
    )?;
    make_executable(&launcher)?;
    Ok(path)
}

/// `Path.chmod(0o755)`.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), PyOsError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| PyOsError::new(error, Some(path)))
}

/// `Path.chmod(0o755)` on Windows only clears the read-only attribute,
/// which a file that was just written does not have.
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), PyOsError> {
    Ok(())
}

/// `appbundle._make_icns`: build the icon with `iconutil`; false when skipped
/// or when it fails.
#[cfg(target_os = "macos")]
fn make_icns(target: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path).any(|dir| {
        fs::metadata(dir.join("iconutil"))
            .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
    });
    if !found {
        return false;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("lf-rs-icon-{}-{stamp}", std::process::id()));
    let built = build_icns(&tmp.join("LocalFlow.iconset"), target);
    let _ = fs::remove_dir_all(&tmp);
    built.unwrap_or(false)
}

/// Write the iconset PNGs and run `iconutil -c icns <iconset> -o <target>`.
#[cfg(target_os = "macos")]
fn build_icns(iconset: &Path, target: &Path) -> std::io::Result<bool> {
    use std::process::{Command, Stdio};

    fs::create_dir_all(iconset)?;
    for base in [16, 32, 128, 256, 512] {
        fs::write(
            iconset.join(format!("icon_{base}x{base}.png")),
            icon::png(base)?,
        )?;
        fs::write(
            iconset.join(format!("icon_{base}x{base}@2x.png")),
            icon::png(base * 2)?,
        )?;
    }
    let output = Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(iconset)
        .arg("-o")
        .arg(target)
        .stdin(Stdio::inherit())
        .output()?;
    Ok(output.status.success() && target.exists())
}

#[cfg(not(target_os = "macos"))]
fn make_icns(_target: &Path) -> bool {
    false
}

/// `appbundle._png`: the app icon (a rounded blue square with a white
/// microphone) as an RGBA PNG, zlib level 9 like Python. The pixels match
/// Python's; the compressed bytes need not.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod icon {
    use std::io::{self, Write};

    use flate2::write::ZlibEncoder;
    use flate2::{Compression, Crc};

    /// Filtered scanlines (filter byte 0, then RGBA) of a `size`² icon.
    pub(super) fn scanlines(size: u32) -> Vec<u8> {
        let side = f64::from(size);
        let half = side / 2.0;
        let radius = side * 0.22;
        let square = |value: f64| value.powf(2.0);
        let mut rows = Vec::with_capacity((size as usize * 4 + 1) * size as usize);
        for y in 0..size {
            rows.push(0);
            let fy = f64::from(y);
            for x in 0..size {
                let fx = f64::from(x);
                let (dx, dy) = ((fx + 0.5 - half).abs(), (fy + 0.5 - half).abs());
                let inner = half - radius;
                let corner = square((dx - inner).max(0.0)) + square((dy - inner).max(0.0));
                if corner > square(radius) {
                    rows.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let (u, v) = ((fx + 0.5) / side - 0.5, (fy + 0.5) / side - 0.5);
                let capsule = u.abs() < 0.11
                    && ((-0.26 < v && v < 0.06)
                        || (square(u.abs()) + square(v + 0.26)) < square(0.11)
                        || (square(u.abs()) + square(v - 0.06)) < square(0.11));
                let ring = (u * u + square(v - 0.06)).powf(0.5);
                let holder = (0.18 < ring && ring < 0.21 && v > 0.06)
                    || (u.abs() < 0.015 && 0.25 < v && v < 0.32)
                    || (u.abs() < 0.1 && 0.31 < v && v < 0.34);
                if capsule || holder {
                    rows.extend_from_slice(&[255, 255, 255, 255]);
                } else {
                    let shade = (70.0 + 90.0 * (fy / side)) as u8;
                    rows.extend_from_slice(&[30, shade, 220, 255]);
                }
            }
        }
        rows
    }

    pub(super) fn crc32(data: &[u8]) -> u32 {
        let mut crc = Crc::new();
        crc.update(data);
        crc.sum()
    }

    pub(super) fn png(size: u32) -> io::Result<Vec<u8>> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut header = Vec::with_capacity(13);
        header.extend_from_slice(&size.to_be_bytes());
        header.extend_from_slice(&size.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&scanlines(size))?;
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut out, b"IHDR", &header);
        chunk(&mut out, b"IDAT", &encoder.finish()?);
        chunk(&mut out, b"IEND", b"");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_pixels_match_python() {
        // zlib.crc32 of the decompressed IDAT of appbundle._png(size).
        for (size, length, crc) in [
            (16, 1040, 0xbd1b_87a4),
            (32, 4128, 0x697d_9274),
            (64, 16448, 0xd7d7_247c),
            (1024, 4_195_328, 0xa57d_866c),
        ] {
            let rows = icon::scanlines(size);
            assert_eq!((rows.len(), icon::crc32(&rows)), (length, crc), "{size}");
        }
    }

    #[test]
    fn png_idat_inflates_to_the_scanlines() {
        use std::io::Read as _;

        let png = icon::png(16).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x10\0\0\0\x10\x08\x06"));
        assert!(png.ends_with(b"\0\0\0\0IEND\xae\x42\x60\x82"));
        // Signature (8) + IHDR chunk (25), then the IDAT chunk.
        let length = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let (data, crc) = (&png[41..41 + length], &png[41 + length..45 + length]);
        assert_eq!(crc, icon::crc32(&png[37..41 + length]).to_be_bytes());
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut inflated)
            .unwrap();
        assert_eq!(inflated, icon::scanlines(16));
        assert!(length < inflated.len() / 2, "{length}");
    }

    #[test]
    fn install_app_writes_the_bundle() {
        let root = tempfile::tempdir().unwrap();
        let path = install_app(root.path(), "/my py/python3", &BUNDLES[1]).unwrap();
        assert_eq!(path, root.path().join("LocalFlow.app"));
        let contents = path.join("Contents");
        let launcher = fs::read_to_string(contents.join("MacOS").join("launcher")).unwrap();
        let newline = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(
            launcher,
            format!(
                "#!/bin/bash{newline}exec '/my py/python3' -m localflow.main_app \"$@\"{newline}"
            )
        );
        let plist = fs::read_to_string(contents.join("Info.plist")).unwrap();
        assert!(plist.contains("\t<key>LSUIElement</key>\n\t<false/>\n"));
        assert!(contents.join("Resources").is_dir());
    }
}
