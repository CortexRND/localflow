#[cfg(target_os = "macos")]
#[path = "../src/mac/accessibility.rs"]
mod accessibility;

fn main() {
    #[cfg(target_os = "macos")]
    accessibility::run();
}
