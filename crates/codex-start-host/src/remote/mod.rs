//! Remote gateway commands. Windows keeps the existing launcher behavior.

use crate::{
    cli::{Cli, Command},
    error::{HostError, Result},
};
use clap::{Args, Subcommand};

#[cfg(unix)]
mod lifecycle;
#[cfg(unix)]
mod output;
#[cfg(unix)]
mod projects;
#[cfg(unix)]
mod history;
#[cfg(unix)]
pub(crate) mod registry;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod sessions;
#[cfg(unix)]
mod settings;
#[cfg(unix)]
mod state;
#[cfg(unix)]
mod terminals;

#[derive(Clone, Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub enum DaemonCommand {
    Start(DaemonOptions),
    /// Restart the daemon with its saved settings, or start it if stopped.
    Restart,
    Run(DaemonOptions),
    Stop,
    Status,
    Install(DaemonOptions),
    Uninstall,
}
#[derive(Clone, Debug, Args, serde::Serialize, serde::Deserialize)]
pub struct DaemonOptions {
    #[arg(long, default_value = "0.0.0.0:47321")]
    pub bind: std::net::SocketAddr,
    #[arg(long)]
    pub no_yggdrasil: bool,
    #[arg(long)]
    pub advertise_host: Option<String>,
}
impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            bind: std::net::SocketAddr::from(([0, 0, 0, 0], 47321)),
            no_yggdrasil: false,
            advertise_host: None,
        }
    }
}
#[derive(Clone, Debug, Args)]
pub struct ConnectArgs {
    #[arg(long)]
    pub host: Option<String>,
}
#[derive(Clone, Debug, Args)]
pub struct DeviceArgs {
    #[command(subcommand)]
    pub command: DeviceCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub enum DeviceCommand {
    List,
    Revoke { id: String },
}
#[derive(Clone, Debug, Args)]
pub struct PasswordArgs {
    #[command(subcommand)]
    pub command: PasswordCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub enum PasswordCommand {
    Rotate,
}

pub fn is_remote_command(command: Option<&Command>) -> bool {
    matches!(
        command,
        Some(
            Command::Daemon(_)
                | Command::Connect(_)
                | Command::Approve { .. }
                | Command::Device(_)
                | Command::ConnectionPassword(_)
        )
    )
}

pub async fn dispatch(cli: &Cli) -> Result<u8> {
    #[cfg(unix)]
    {
        lifecycle::dispatch(cli).await
    }
    #[cfg(not(unix))]
    {
        let _ = cli;
        Err(HostError::Usage(
            "remote daemon requires Linux or macOS".into(),
        ))
    }
}

#[cfg(unix)]
fn error(error: impl std::fmt::Display) -> HostError {
    HostError::Runtime(error.to_string())
}

#[cfg(unix)]
mod files;

mod thread_access;
