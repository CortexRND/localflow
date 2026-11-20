//! Compatibility shim: the `xattr` 0.2 API, delegated to upstream `xattr` 1.x.
//!
//! Used only by `rust/lf-desktop` through `[patch.crates-io]`; nothing else in
//! the repository builds it.
//!
//! **Why it exists.** GPUI 0.2.2 depends on `zed-async-tar`, which requires
//! `xattr = "0.2"`. The last 0.2 release (0.2.3) reads `libc::ENOATTR` on Linux,
//! and libc 0.2.191 removed that alias. LocalFlow's keyring backend
//! (`dbus-secret-service` 4.2, through `lf-core`) requires libc 0.2.191 or
//! newer, so xattr 0.2.3 no longer compiles in this dependency graph.
//!
//! **What it does.** Nothing of its own. It re-exports upstream xattr 1.x,
//! the maintained successor, whose Linux backend uses `rustix` and whose macOS
//! and BSD backends are upstream's own. The tar crate calls only [`set`],
//! with the same signature in both major versions:
//! `set<N: AsRef<OsStr>, P: AsRef<Path>>(path: P, name: N, value: &[u8]) -> io::Result<()>`.
//! No unsafe code lives here.
//!
//! **When to delete it.** When GPUI's tar dependency moves to xattr 1.x,
//! remove this directory and the `[patch.crates-io]` entry in
//! `rust/lf-desktop/Cargo.toml`.

pub use upstream::*;

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    /// The 0.2 call shape `zed-async-tar` uses still type-checks and runs.
    #[test]
    fn set_keeps_the_0_2_signature() {
        fn takes_tar_shape(f: fn(&Path, &OsStr, &[u8]) -> std::io::Result<()>) {
            let _ = f;
        }
        takes_tar_shape(|p, n, v| super::set(p, n, v));
        let _: bool = super::SUPPORTED_PLATFORM;
    }

    /// On Linux and macOS a real attribute can be written and read back
    /// (skipped when the filesystem refuses user attributes, e.g. tmpfs).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn round_trips_a_user_attribute() {
        let dir = std::env::temp_dir().join(format!("xattr-shim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        let name = if cfg!(target_os = "linux") {
            "user.lf_shim"
        } else {
            "lf_shim"
        };
        match super::set(&file, name, b"ok") {
            Ok(()) => assert_eq!(
                super::get(&file, name).unwrap().as_deref(),
                Some(&b"ok"[..])
            ),
            Err(err) => eprintln!("filesystem refuses xattrs here ({err}); skipping read-back"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
