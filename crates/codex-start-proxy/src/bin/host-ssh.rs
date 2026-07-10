//! Container-side executable compatible with Git's `GIT_SSH` interface.

use std::{path::PathBuf, time::Duration};

use clap::Parser;
use codex_start_proxy::{
    allowlist::Authority,
    auth::AuthToken,
    connect::open_connect_tunnel,
    host_ssh::{host_ssh_client_stream, is_git_ssh_variant_probe, run_host_ssh_client},
    relay::RelayConfig,
};

#[derive(Debug, Parser)]
#[command(
    name = "codex-start-host-ssh",
    about = "Forward an OpenSSH invocation to an authenticated host bridge",
    disable_help_flag = true
)]
struct Cli {
    #[arg(long, env = "CODEX_START_HOST_SSH_ADDR")]
    bridge: String,
    #[arg(long, env = "CODEX_START_HOST_SSH_TOKEN_FILE")]
    token_file: PathBuf,
    #[arg(long, env = "CODEX_START_HOST_SSH_PROXY")]
    proxy: Option<String>,
    #[arg(
        long,
        env = "CODEX_START_HOST_SSH_PROXY_TOKEN_FILE",
        requires = "proxy"
    )]
    proxy_token_file: Option<PathBuf>,
    #[arg(
        long,
        env = "CODEX_START_HOST_SSH_CONNECT_TIMEOUT",
        default_value_t = 10
    )]
    connect_timeout_seconds: u64,
    #[arg(
        long,
        env = "CODEX_START_HOST_SSH_HANDSHAKE_TIMEOUT",
        default_value_t = 5
    )]
    handshake_timeout_seconds: u64,
    #[arg(long, env = "CODEX_START_HOST_SSH_IDLE_TIMEOUT", default_value_t = 300)]
    idle_timeout_seconds: u64,
    /// Exact arguments normally passed to OpenSSH by Git.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    argv: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if is_git_ssh_variant_probe(&cli.argv) {
        return;
    }
    let result = async {
        let token = AuthToken::from_file(&cli.token_file)?;
        let config = RelayConfig {
            connect_timeout: Duration::from_secs(cli.connect_timeout_seconds),
            handshake_timeout: Duration::from_secs(cli.handshake_timeout_seconds),
            idle_timeout: Duration::from_secs(cli.idle_timeout_seconds),
            ..RelayConfig::default()
        };
        let exit_code = if let Some(proxy) = cli.proxy {
            let target = Authority::parse(&cli.bridge, None)?;
            let proxy_token = cli
                .proxy_token_file
                .as_deref()
                .map(AuthToken::from_file)
                .transpose()?;
            let stream =
                open_connect_tunnel(&proxy, &target, proxy_token.as_ref(), &config).await?;
            host_ssh_client_stream(
                stream,
                tokio::io::stdin(),
                tokio::io::stdout(),
                tokio::io::stderr(),
                &token,
                cli.argv,
                &config,
            )
            .await?
        } else {
            run_host_ssh_client(&cli.bridge, &token, cli.argv, &config).await?
        };
        Ok::<u8, Box<dyn std::error::Error>>(exit_code)
    }
    .await;
    match result {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(i32::from(exit_code)),
        Err(error) => {
            eprintln!("codex-start-host-ssh: {error}");
            std::process::exit(255);
        }
    }
}
