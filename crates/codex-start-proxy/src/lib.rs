//! Network and host-transport primitives used by codex-start.

pub mod allowlist;
pub mod auth;
pub mod browser;
pub mod connect;
#[cfg(unix)]
pub mod container_init;
#[cfg(windows)]
#[path = "container_init_windows.rs"]
pub mod container_init;
pub mod egress;
pub mod host_ssh;
pub mod relay;
