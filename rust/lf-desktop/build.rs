//! Relative native-library search paths for the `runtime` build.
//!
//! The runtime feature links the shared Sherpa-ONNX / ONNX Runtime libraries.
//! The rpath link args that `sherpa-onnx-sys`, `lf-stt` and `lf-ui` emit apply
//! only to their own targets, so without this the `lf-desktop` binary has no
//! search path and runs only under cargo (which sets the loader path).
//!
//! - Linux: `$ORIGIN` finds the libraries cargo copies next to the binary
//!   (`target/<profile>/`); `$ORIGIN/../lib` is the preview package layout
//!   (`bin/lf-desktop`, `lib/*.so`). `scripts/build_gpui_package.sh` asserts
//!   the binary carries exactly these entries.
//! - macOS: `@loader_path` for the cargo sibling dylibs and
//!   `@loader_path/../Frameworks` for the app bundle. The package script
//!   replaces them with `@executable_path/../Frameworks`, as
//!   `scripts/build_rust_app.sh` does for `lf-hub`.
//!
//! The default (front-end only) build links no native speech runtime and gets
//! no extra search path.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_RUNTIME").is_none() {
        return;
    }
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
        }
        Ok("macos") => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../Frameworks");
        }
        _ => {}
    }
}
