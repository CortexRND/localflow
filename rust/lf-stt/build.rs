fn main() {
    if std::env::var_os("CARGO_FEATURE_PARAKEET").is_some() {
        match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
            Ok("linux") => {
                println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
                println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/..");
            }
            Ok("macos") => {
                println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
                println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/..");
            }
            _ => {}
        }
    }
}
