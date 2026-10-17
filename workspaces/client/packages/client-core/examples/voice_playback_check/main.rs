//! The check drives macOS frameworks; other platforms build an empty entry point.
#[cfg(target_os = "macos")]
mod check;

fn main() {
    #[cfg(target_os = "macos")]
    check::main();
}
