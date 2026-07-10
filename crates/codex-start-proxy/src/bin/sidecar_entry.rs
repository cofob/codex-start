//! Platform entry point for the Linux-container sidecar.

#[cfg(unix)]
include!("sidecar.rs");

#[cfg(windows)]
fn main() {
    eprintln!("codex-start-sidecar runs inside Linux containers and is unavailable on Windows");
    std::process::exit(1);
}
