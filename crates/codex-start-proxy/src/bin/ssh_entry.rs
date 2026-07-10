//! Platform entry point for the in-container OpenSSH wrapper.

#[cfg(unix)]
include!("ssh.rs");

#[cfg(windows)]
fn main() {
    eprintln!("codex-start-ssh runs inside Linux containers and is unavailable on Windows");
    std::process::exit(1);
}
