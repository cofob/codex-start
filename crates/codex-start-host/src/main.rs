//! codex-start command-line entry point.

mod app;
mod assets;
mod cli;
mod command;
mod configuration;
mod content_hash;
mod editor;
mod environments;
mod error;
mod forwarding;
mod git;
mod home;
mod host_services;
mod init_spec;
mod launch_plan;
mod locking;
mod networking;
mod paths;
mod runtime;
mod secrets;

use std::process::ExitCode;

use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli::run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("codex-start: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
