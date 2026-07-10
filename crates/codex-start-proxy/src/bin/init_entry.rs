//! Platform entry point for the Linux-container init helper.

#[cfg(unix)]
include!("init.rs");

#[cfg(windows)]
fn main() {
    eprintln!("codex-start-init runs inside Linux containers and is unavailable on Windows");
    std::process::exit(1);
}
