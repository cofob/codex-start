//! Sidecar process entry point.

use std::{net::SocketAddr, os::unix::ffi::OsStringExt, path::PathBuf, time::Duration};

use clap::{Parser, Subcommand, ValueEnum};
use codex_start_proxy::{
    allowlist::{AddressPolicy, AllowList},
    auth::AuthToken,
    browser::{
        BrowserOpenConfig, serve_browser_open, serve_oauth_callback_target,
        serve_oauth_host_listener,
    },
    egress::{EgressConfig, serve_egress},
    host_ssh::{HostSshConfig, serve_host_ssh},
    relay::{
        RelayConfig, RelayTarget, bind_unix_listener, serve_authenticated_tcp, serve_tcp_bridge,
        serve_unix_bridge,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "codex-start-sidecar",
    about = "Rust-only egress and authenticated relay sidecar"
)]
struct Cli {
    /// Log encoding written to stderr.
    #[arg(long, value_enum, default_value_t = LogFormat::Json, global = true)]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve an allow-listed HTTP forward and CONNECT proxy.
    Egress(EgressArgs),
    /// Authenticate TCP clients and relay them to a TCP service.
    TcpToTcp(ServerRelayArgs),
    /// Authenticate TCP clients and relay them to a Unix socket.
    TcpToUnix(UnixServerRelayArgs),
    /// Accept local TCP streams and bridge them to an authenticated relay.
    TcpBridge(ClientRelayArgs),
    /// Accept local Unix streams and bridge them to an authenticated relay.
    UnixBridge(UnixClientRelayArgs),
    /// Probe an egress sidecar health endpoint.
    Healthcheck {
        #[arg(long, default_value = "127.0.0.1:3128")]
        proxy: SocketAddr,
        #[arg(long, default_value_t = 5)]
        timeout_seconds: u64,
    },
    /// Verify the running PID 1 identity and effective/permitted capabilities.
    IdentityCheck {
        #[arg(long, default_value_t = 65_532)]
        uid: u32,
        #[arg(long, default_value_t = 65_532)]
        gid: u32,
    },
    /// Execute strictly validated, allow-listed OpenSSH invocations on the host.
    HostSsh(HostSshArgs),
    /// Run an authenticated, allow-listed host browser opener.
    BrowserOpen(BrowserOpenArgs),
    /// Listen on host loopback and reverse OAuth callbacks to a container relay.
    OauthListener(OauthListenerArgs),
    /// Accept authenticated callbacks and forward them to container loopback.
    OauthTarget(OauthTargetArgs),
}

#[derive(Debug, clap::Args)]
struct EgressArgs {
    #[arg(long, default_value = "0.0.0.0:3128")]
    listen: SocketAddr,
    /// Host rule, for example `api.openai.com:443` or `*.example.com`.
    #[arg(long = "allow", required = true)]
    allow: Vec<String>,
    /// Allowed authority that may resolve to private/reserved addresses.
    #[arg(long = "allow-private")]
    allow_private: Vec<String>,
    /// Optional bearer token file for HTTP proxy authentication.
    #[arg(
        long,
        conflicts_with = "auth_token_env",
        required_unless_present = "auth_token_env"
    )]
    auth_token_file: Option<PathBuf>,
    /// Environment name populated by the root init process after engine
    /// inspection, immediately before it drops to the sidecar UID.
    #[arg(
        long,
        conflicts_with = "auth_token_file",
        required_unless_present = "auth_token_file"
    )]
    auth_token_env: Option<String>,
    #[arg(long, default_value_t = 256)]
    max_connections: usize,
    #[arg(long, default_value_t = 65_536)]
    max_header_bytes: usize,
    #[arg(long, default_value_t = 10)]
    header_timeout_seconds: u64,
    #[arg(long, default_value_t = 10)]
    connect_timeout_seconds: u64,
    #[arg(long, default_value_t = 300)]
    idle_timeout_seconds: u64,
}

#[derive(Debug, clap::Args)]
struct RelayLimits {
    #[arg(long, default_value_t = 128)]
    max_connections: usize,
    #[arg(long, default_value_t = 10)]
    connect_timeout_seconds: u64,
    #[arg(long, default_value_t = 5)]
    handshake_timeout_seconds: u64,
    #[arg(long, default_value_t = 300)]
    idle_timeout_seconds: u64,
}

impl RelayLimits {
    fn config(&self) -> RelayConfig {
        RelayConfig {
            max_connections: self.max_connections,
            connect_timeout: Duration::from_secs(self.connect_timeout_seconds),
            handshake_timeout: Duration::from_secs(self.handshake_timeout_seconds),
            idle_timeout: Duration::from_secs(self.idle_timeout_seconds),
        }
    }
}

#[derive(Debug, clap::Args)]
struct ServerRelayArgs {
    #[arg(long)]
    listen: SocketAddr,
    /// Upstream may be a hostname or IP and port.
    #[arg(long)]
    target: String,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct UnixServerRelayArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    target: PathBuf,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct ClientRelayArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct UnixClientRelayArgs {
    #[arg(long)]
    listen: PathBuf,
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct HostSshArgs {
    #[arg(long)]
    listen: SocketAddr,
    /// Explicit destination rule such as `github.com:22`.
    #[arg(long = "allow", required = true)]
    allow: Vec<String>,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long, default_value = "ssh")]
    ssh_program: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct BrowserOpenArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long = "allow", required = true)]
    allow: Vec<String>,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    opener_program: PathBuf,
    #[arg(long = "opener-arg")]
    opener_args: Vec<String>,
    #[arg(long, default_value_t = 10)]
    opener_timeout_seconds: u64,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct OauthListenerArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct OauthTargetArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    callback: SocketAddr,
    #[arg(long)]
    token_file: PathBuf,
    #[command(flatten)]
    limits: RelayLimits,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    initialize_logging(cli.log_format);
    if let Err(error) = run(cli.command).await {
        tracing::error!(%error, "sidecar terminated");
        std::process::exit(1);
    }
}

fn initialize_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Json => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .try_init();
        }
        LogFormat::Text => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}

type AnyError = Box<dyn std::error::Error>;

async fn run(command: Command) -> Result<(), AnyError> {
    match command {
        Command::Egress(args) => run_egress(args).await?,
        Command::TcpToTcp(args) => {
            run_authenticated(
                args.listen,
                RelayTarget::Tcp(args.target),
                args.token_file,
                args.limits,
            )
            .await?;
        }
        Command::TcpToUnix(args) => {
            run_authenticated(
                args.listen,
                RelayTarget::Unix(args.target),
                args.token_file,
                args.limits,
            )
            .await?;
        }
        Command::TcpBridge(args) => run_tcp_bridge(args).await?,
        Command::UnixBridge(args) => run_unix_bridge(args).await?,
        Command::Healthcheck {
            proxy,
            timeout_seconds,
        } => healthcheck(proxy, Duration::from_secs(timeout_seconds)).await?,
        Command::IdentityCheck { uid, gid } => {
            verify_process_status(&std::fs::read_to_string("/proc/1/status")?, uid, gid)?;
        }
        Command::HostSsh(args) => run_host_ssh(args).await?,
        Command::BrowserOpen(args) => run_browser_open(args).await?,
        Command::OauthListener(args) => run_oauth_listener(args).await?,
        Command::OauthTarget(args) => run_oauth_target(args).await?,
    }
    Ok(())
}

fn verify_process_status(
    status: &str,
    target_user: u32,
    target_group: u32,
) -> Result<(), AnyError> {
    let value = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_ascii_whitespace().next())
    };
    let uid = value("Uid:")
        .ok_or("process status has no UID")?
        .parse::<u32>()?;
    let gid = value("Gid:")
        .ok_or("process status has no GID")?
        .parse::<u32>()?;
    let permitted = u64::from_str_radix(
        value("CapPrm:").ok_or("process status has no permitted capabilities")?,
        16,
    )?;
    let effective = u64::from_str_radix(
        value("CapEff:").ok_or("process status has no effective capabilities")?,
        16,
    )?;
    if uid != target_user || gid != target_group || permitted != 0 || effective != 0 {
        return Err(format!(
            "PID 1 identity mismatch: uid={uid} gid={gid} CapPrm={permitted:x} CapEff={effective:x}"
        )
        .into());
    }
    Ok(())
}

async fn run_egress(args: EgressArgs) -> Result<(), AnyError> {
    let auth_token = match (
        args.auth_token_file.as_deref(),
        args.auth_token_env.as_deref(),
    ) {
        (Some(path), None) => Some(AuthToken::from_file(path)?),
        (None, Some(name)) => {
            if !valid_environment_name(name) {
                return Err("invalid egress authentication environment name".into());
            }
            let value = std::env::var_os(name)
                .ok_or("egress authentication environment variable is missing")?;
            Some(AuthToken::new(value.into_vec())?)
        }
        (None, None) => None,
        (Some(_), Some(_)) => return Err("egress authentication sources conflict".into()),
    };
    let config = EgressConfig {
        allowlist: AllowList::parse(args.allow.iter().map(String::as_str))?,
        address_policy: AddressPolicy {
            private_authorities: AllowList::parse(args.allow_private.iter().map(String::as_str))?,
        },
        auth_token,
        max_connections: args.max_connections,
        max_header_bytes: args.max_header_bytes,
        header_timeout: Duration::from_secs(args.header_timeout_seconds),
        connect_timeout: Duration::from_secs(args.connect_timeout_seconds),
        idle_timeout: Duration::from_secs(args.idle_timeout_seconds),
    };
    let listener = TcpListener::bind(args.listen).await?;
    serve_egress(listener, config, shutdown_signal()).await?;
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

async fn run_authenticated(
    listen: SocketAddr,
    target: RelayTarget,
    token_file: PathBuf,
    limits: RelayLimits,
) -> Result<(), AnyError> {
    let listener = TcpListener::bind(listen).await?;
    let token = AuthToken::from_file(&token_file)?;
    serve_authenticated_tcp(listener, target, token, limits.config(), shutdown_signal()).await?;
    Ok(())
}

async fn run_tcp_bridge(args: ClientRelayArgs) -> Result<(), AnyError> {
    let listener = TcpListener::bind(args.listen).await?;
    let token = AuthToken::from_file(&args.token_file)?;
    serve_tcp_bridge(
        listener,
        args.remote,
        token,
        args.limits.config(),
        shutdown_signal(),
    )
    .await?;
    Ok(())
}

async fn run_unix_bridge(args: UnixClientRelayArgs) -> Result<(), AnyError> {
    let listener = bind_unix_listener(&args.listen)?;
    let token = AuthToken::from_file(&args.token_file)?;
    let result = serve_unix_bridge(
        listener,
        args.remote,
        token,
        args.limits.config(),
        shutdown_signal(),
    )
    .await;
    if let Err(error) = std::fs::remove_file(&args.listen)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %args.listen.display(), %error, "failed to remove relay socket");
    }
    result?;
    Ok(())
}

async fn run_host_ssh(args: HostSshArgs) -> Result<(), AnyError> {
    let listener = TcpListener::bind(args.listen).await?;
    let token = AuthToken::from_file(&args.token_file)?;
    let config = HostSshConfig {
        allowlist: AllowList::parse(args.allow.iter().map(String::as_str))?,
        ssh_program: args.ssh_program,
        relay: args.limits.config(),
    };
    serve_host_ssh(listener, token, config, shutdown_signal()).await?;
    Ok(())
}

async fn run_browser_open(args: BrowserOpenArgs) -> Result<(), AnyError> {
    let listener = TcpListener::bind(args.listen).await?;
    let token = AuthToken::from_file(&args.token_file)?;
    let config = BrowserOpenConfig {
        allowlist: AllowList::parse(args.allow.iter().map(String::as_str))?,
        opener_program: args.opener_program,
        opener_args: args.opener_args.into_iter().map(Into::into).collect(),
        relay: args.limits.config(),
        opener_timeout: Duration::from_secs(args.opener_timeout_seconds),
    };
    serve_browser_open(listener, token, config, shutdown_signal()).await?;
    Ok(())
}

async fn run_oauth_listener(args: OauthListenerArgs) -> Result<(), AnyError> {
    let listener = TcpListener::bind(args.listen).await?;
    let token = AuthToken::from_file(&args.token_file)?;
    serve_oauth_host_listener(
        listener,
        args.remote,
        token,
        args.limits.config(),
        shutdown_signal(),
    )
    .await?;
    Ok(())
}

async fn run_oauth_target(args: OauthTargetArgs) -> Result<(), AnyError> {
    let listener = TcpListener::bind(args.listen).await?;
    let token = AuthToken::from_file(&args.token_file)?;
    serve_oauth_callback_target(
        listener,
        args.callback,
        token,
        args.limits.config(),
        shutdown_signal(),
    )
    .await?;
    Ok(())
}

async fn healthcheck(proxy: SocketAddr, limit: Duration) -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(limit, async {
        let mut stream = TcpStream::connect(proxy).await?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        if !response.starts_with(b"HTTP/1.1 200 ") {
            return Err(std::io::Error::other("unhealthy proxy response"));
        }
        Ok::<(), std::io::Error>(())
    })
    .await??;
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler must install");
    tokio::select! {
        result = interrupt => {
            if let Err(error) = result {
                tracing::warn!(%error, "failed to listen for Ctrl-C");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::verify_process_status;

    #[test]
    fn process_identity_probe_requires_non_root_and_zero_capabilities() {
        let valid = "Uid:\t65532\t65532\t65532\t65532\nGid:\t65532\t65532\t65532\t65532\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n";
        assert!(verify_process_status(valid, 65_532, 65_532).is_ok());
        assert!(
            verify_process_status(
                &valid.replace("CapEff:\t0000000000000000", "CapEff:\t0000000000000080"),
                65_532,
                65_532
            )
            .is_err()
        );
        assert!(
            verify_process_status(&valid.replace("Uid:\t65532", "Uid:\t0"), 65_532, 65_532)
                .is_err()
        );
    }
}
