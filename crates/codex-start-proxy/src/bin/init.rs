//! Container init and local authenticated-tunnel helper.

use std::{
    ffi::OsString, net::SocketAddr, os::unix::ffi::OsStringExt, path::PathBuf, time::Duration,
};

use clap::{Parser, Subcommand};
use codex_start_proxy::{
    allowlist::Authority,
    auth::AuthToken,
    browser::{request_browser_open, request_browser_open_stream, serve_oauth_callback_target},
    connect::{
        ConnectBridgeConfig, HttpProxyBridgeConfig, TcpForwardConfig, connect_stdio,
        open_connect_tunnel, serve_connect_bridge, serve_http_proxy_bridge,
        serve_tcp_authenticated_connect_bridge, serve_tcp_forward,
        serve_unix_authenticated_connect_bridge,
    },
    container_init::{
        CommandSpec, DirectInitOptions, InitError, SshSetup, direct_spec, load_init_spec,
        parse_prepare_json, run_and_exec,
    },
    host_ssh::{host_ssh_client_stream, is_git_ssh_variant_probe, run_host_ssh_client},
    relay::{RelayConfig, bind_unix_listener, serve_tcp_bridge, serve_unix_bridge},
};
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(
    name = "codex-start-init",
    about = "Container init and authenticated local tunnel helper"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Load mounted secrets, run argv-only preparation, and exec the workload.
    #[command(visible_alias = "exec")]
    Run(RunArgs),
    /// Expose a credential-free loopback proxy backed by authenticated egress.
    HttpProxy(HttpProxyArgs),
    /// Expose a local TCP endpoint through an authenticated host relay.
    TcpBridge(BridgeArgs),
    /// Expose a local Unix endpoint through an authenticated host relay.
    UnixBridge(UnixBridgeArgs),
    /// Expose a local TCP endpoint through the allow-list HTTP CONNECT proxy.
    ConnectBridge(ConnectBridgeArgs),
    /// Expose a loopback TCP endpoint through one fixed direct target.
    TcpForward(TcpForwardArgs),
    /// Relay stdin/stdout through HTTP CONNECT (for OpenSSH `ProxyCommand`).
    ConnectStdio(ConnectStdioArgs),
    /// Request a URL from the authenticated host browser opener.
    #[command(name = "browser-open", visible_alias = "open-url")]
    OpenUrl(OpenUrlArgs),
    /// Run an OpenSSH/Git invocation through the authenticated host bridge.
    HostSsh(HostSshArgs),
    /// Forward authenticated host OAuth callbacks to container loopback.
    OauthTarget(OauthTargetArgs),
}

#[derive(Debug, clap::Args)]
struct HttpProxyArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    proxy: String,
    #[arg(
        long,
        conflicts_with = "token_env",
        required_unless_present = "token_env"
    )]
    token_file: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "token_file",
        required_unless_present = "token_file"
    )]
    token_env: Option<String>,
    #[arg(long, default_value_t = 65_536)]
    max_header_bytes: usize,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Versioned JSON init specification. Cannot be mixed with direct command options.
    #[arg(long, env = "CODEX_START_INIT_SPEC")]
    spec: Option<PathBuf>,
    #[arg(long)]
    uid: Option<u32>,
    #[arg(long)]
    gid: Option<u32>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// JSON object mapping environment names to files under `--secret-root`.
    #[arg(long, env = "CODEX_START_SECRET_MAP")]
    secret_map: Option<PathBuf>,
    #[arg(long, default_value = "/run/codex-start/secrets")]
    secret_root: PathBuf,
    #[arg(long)]
    allow_insecure_secret_permissions: bool,
    #[arg(long)]
    clear_environment: bool,
    /// Container-owned writable root to chown before dropping privileges.
    #[arg(long = "ownership-path")]
    ownership_paths: Vec<PathBuf>,
    /// Read supported SSH config files from this directory.
    #[arg(long, requires = "ssh_destination")]
    ssh_source: Option<PathBuf>,
    /// Materialize SSH config into this workload `.ssh` directory.
    #[arg(long, requires = "ssh_source")]
    ssh_destination: Option<PathBuf>,
    /// JSON argv array, repeatable and executed without a shell.
    #[arg(long = "prepare-json", value_parser = parse_prepare)]
    prepare: Vec<CommandSpec>,
    /// Final program and arguments after `--`.
    #[arg(last = true)]
    command: Vec<OsString>,
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
struct BridgeArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    /// Optional egress proxy used to CONNECT to the authenticated relay.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, requires = "proxy")]
    proxy_token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct UnixBridgeArgs {
    #[arg(long)]
    listen: PathBuf,
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    /// Optional egress proxy used to CONNECT to the authenticated relay.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, requires = "proxy")]
    proxy_token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct ConnectBridgeArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    proxy: String,
    #[arg(long)]
    target: String,
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct TcpForwardArgs {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    target: String,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct ConnectStdioArgs {
    #[arg(long)]
    proxy: String,
    #[arg(long)]
    target: String,
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
}

#[derive(Debug, clap::Args)]
struct OpenUrlArgs {
    #[arg(long)]
    bridge: String,
    #[arg(long)]
    token_file: PathBuf,
    /// Optional egress proxy used to CONNECT to the browser bridge.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, requires = "proxy")]
    proxy_token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
    url: String,
}

#[derive(Debug, clap::Args)]
struct HostSshArgs {
    #[arg(long)]
    remote: String,
    #[arg(long)]
    token_file: PathBuf,
    /// Optional egress proxy used to CONNECT to the host SSH bridge.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, requires = "proxy")]
    proxy_token_file: Option<PathBuf>,
    #[command(flatten)]
    limits: RelayLimits,
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    argv: Vec<String>,
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

fn parse_prepare(input: &str) -> Result<CommandSpec, String> {
    parse_prepare_json(input).map_err(|error| error.to_string())
}

fn main() {
    let cli = Cli::parse();
    let is_host_ssh = matches!(&cli.command, Command::HostSsh(_));
    match run(cli.command) {
        Ok(Some(exit_code)) if exit_code != 0 => std::process::exit(i32::from(exit_code)),
        Ok(_) => {}
        Err(error) => {
            eprintln!("codex-start-init: {error}");
            std::process::exit(if is_host_ssh { 255 } else { 1 });
        }
    }
}

type AnyError = Box<dyn std::error::Error>;

fn run(command: Command) -> Result<Option<u8>, AnyError> {
    let exit_code = match command {
        Command::Run(args) => {
            run_workload(args)?;
            None
        }
        Command::HttpProxy(args) => {
            run_http_proxy(args)?;
            None
        }
        Command::TcpBridge(args) => {
            run_tcp_bridge(args)?;
            None
        }
        Command::UnixBridge(args) => {
            run_unix_bridge(args)?;
            None
        }
        Command::ConnectBridge(args) => {
            run_connect_bridge(args)?;
            None
        }
        Command::TcpForward(args) => {
            run_tcp_forward(&args)?;
            None
        }
        Command::ConnectStdio(args) => {
            run_connect_stdio(&args)?;
            None
        }
        Command::OpenUrl(args) => {
            run_browser_client(args)?;
            None
        }
        Command::HostSsh(args) => Some(run_host_ssh(args)?),
        Command::OauthTarget(args) => {
            run_oauth_target(args)?;
            None
        }
    };
    Ok(exit_code)
}

fn run_http_proxy(args: HttpProxyArgs) -> Result<(), AnyError> {
    if !args.listen.ip().is_loopback() || args.listen.port() == 0 {
        return Err(codex_start_proxy::connect::ConnectError::InvalidConfig(
            "listener must use loopback and a non-zero port".to_owned(),
        )
        .into());
    }
    let auth_token = match (args.token_file.as_deref(), args.token_env.as_deref()) {
        (Some(path), None) => AuthToken::from_file(path)?,
        (None, Some(name)) => {
            let value = std::env::var_os(name)
                .ok_or("HTTP proxy authentication environment variable is missing")?;
            AuthToken::new(value.into_vec())?
        }
        _ => return Err("exactly one HTTP proxy authentication source is required".into()),
    };
    let config = HttpProxyBridgeConfig {
        proxy: args.proxy,
        auth_token,
        relay: args.limits.config(),
        max_header_bytes: args.max_header_bytes,
    };
    runtime()?.block_on(async move {
        let listener = TcpListener::bind(args.listen).await?;
        serve_http_proxy_bridge(listener, config, shutdown_signal()).await
    })?;
    Ok(())
}

fn run_tcp_bridge(args: BridgeArgs) -> Result<(), AnyError> {
    let token = AuthToken::from_file(&args.token_file)?;
    if let Some(proxy) = args.proxy {
        let target = Authority::parse(&args.remote, None)?;
        let proxy_token = load_optional_token(args.proxy_token_file.as_deref())?;
        runtime()?.block_on(async move {
            let listener = TcpListener::bind(args.listen).await?;
            serve_tcp_authenticated_connect_bridge(
                listener,
                proxy,
                target,
                proxy_token,
                token,
                args.limits.config(),
                shutdown_signal(),
            )
            .await
        })?;
    } else {
        runtime()?.block_on(async move {
            let listener = TcpListener::bind(args.listen).await?;
            serve_tcp_bridge(
                listener,
                args.remote,
                token,
                args.limits.config(),
                shutdown_signal(),
            )
            .await
        })?;
    }
    Ok(())
}

fn run_unix_bridge(args: UnixBridgeArgs) -> Result<(), AnyError> {
    let listener = bind_unix_listener(&args.listen)?;
    let token = AuthToken::from_file(&args.token_file)?;
    if let Some(proxy) = args.proxy {
        let target = Authority::parse(&args.remote, None)?;
        let proxy_token = load_optional_token(args.proxy_token_file.as_deref())?;
        runtime()?.block_on(async move {
            let result = serve_unix_authenticated_connect_bridge(
                listener,
                proxy,
                target,
                proxy_token,
                token,
                args.limits.config(),
                shutdown_signal(),
            )
            .await;
            let _ = std::fs::remove_file(&args.listen);
            result
        })?;
    } else {
        runtime()?.block_on(async move {
            let result = serve_unix_bridge(
                listener,
                args.remote,
                token,
                args.limits.config(),
                shutdown_signal(),
            )
            .await;
            let _ = std::fs::remove_file(&args.listen);
            result
        })?;
    }
    Ok(())
}

fn run_connect_bridge(args: ConnectBridgeArgs) -> Result<(), AnyError> {
    let target = Authority::parse(&args.target, None)?;
    let auth_token = load_optional_token(args.token_file.as_deref())?;
    let config = ConnectBridgeConfig {
        proxy: args.proxy,
        target,
        auth_token,
        relay: args.limits.config(),
    };
    runtime()?.block_on(async move {
        if !args.listen.ip().is_loopback() || args.listen.port() == 0 {
            return Err(codex_start_proxy::connect::ConnectError::InvalidConfig(
                "listener must use loopback and a non-zero port".to_owned(),
            ));
        }
        let listener = TcpListener::bind(args.listen).await?;
        serve_connect_bridge(listener, config, shutdown_signal()).await
    })?;
    Ok(())
}

fn run_tcp_forward(args: &TcpForwardArgs) -> Result<(), AnyError> {
    if !args.listen.ip().is_loopback() || args.listen.port() == 0 {
        return Err(codex_start_proxy::connect::ConnectError::InvalidConfig(
            "listener must use loopback and a non-zero port".to_owned(),
        )
        .into());
    }
    let config = TcpForwardConfig {
        target: Authority::parse(&args.target, None)?,
        relay: args.limits.config(),
    };
    runtime()?.block_on(async move {
        let listener = TcpListener::bind(args.listen).await?;
        serve_tcp_forward(listener, config, shutdown_signal()).await
    })?;
    Ok(())
}

fn run_connect_stdio(args: &ConnectStdioArgs) -> Result<(), AnyError> {
    let target = Authority::parse(&args.target, None)?;
    let auth_token = load_optional_token(args.token_file.as_deref())?;
    runtime()?.block_on(connect_stdio(
        &args.proxy,
        &target,
        auth_token.as_ref(),
        &args.limits.config(),
    ))?;
    Ok(())
}

fn run_browser_client(args: OpenUrlArgs) -> Result<(), AnyError> {
    let token = AuthToken::from_file(&args.token_file)?;
    if let Some(proxy) = args.proxy {
        let target = Authority::parse(&args.bridge, None)?;
        let proxy_token = load_optional_token(args.proxy_token_file.as_deref())?;
        runtime()?.block_on(async move {
            let limits = args.limits.config();
            let stream =
                open_connect_tunnel(&proxy, &target, proxy_token.as_ref(), &limits).await?;
            request_browser_open_stream(stream, &token, &args.url, limits.handshake_timeout)
                .await?;
            Ok::<(), AnyError>(())
        })?;
    } else {
        runtime()?.block_on(request_browser_open(
            &args.bridge,
            &token,
            &args.url,
            &args.limits.config(),
        ))?;
    }
    Ok(())
}

fn run_host_ssh(args: HostSshArgs) -> Result<u8, AnyError> {
    if is_git_ssh_variant_probe(&args.argv) {
        return Ok(0);
    }
    let token = AuthToken::from_file(&args.token_file)?;
    if let Some(proxy) = args.proxy {
        let target = Authority::parse(&args.remote, None)?;
        let proxy_token = load_optional_token(args.proxy_token_file.as_deref())?;
        runtime()?.block_on(async move {
            let limits = args.limits.config();
            let stream =
                open_connect_tunnel(&proxy, &target, proxy_token.as_ref(), &limits).await?;
            let exit_code = host_ssh_client_stream(
                stream,
                tokio::io::stdin(),
                tokio::io::stdout(),
                tokio::io::stderr(),
                &token,
                args.argv,
                &limits,
            )
            .await?;
            Ok::<u8, AnyError>(exit_code)
        })
    } else {
        runtime()?
            .block_on(run_host_ssh_client(
                &args.remote,
                &token,
                args.argv,
                &args.limits.config(),
            ))
            .map_err(Into::into)
    }
}

fn run_oauth_target(args: OauthTargetArgs) -> Result<(), AnyError> {
    let token = AuthToken::from_file(&args.token_file)?;
    runtime()?.block_on(async move {
        let listener = TcpListener::bind(args.listen)
            .await
            .map_err(codex_start_proxy::browser::OAuthTunnelError::Bind)?;
        serve_oauth_callback_target(
            listener,
            args.callback,
            token,
            args.limits.config(),
            shutdown_signal(),
        )
        .await
    })?;
    Ok(())
}

fn load_optional_token(
    path: Option<&std::path::Path>,
) -> Result<Option<AuthToken>, codex_start_proxy::auth::AuthTokenError> {
    path.map(AuthToken::from_file).transpose()
}

fn run_workload(args: RunArgs) -> Result<(), InitError> {
    if let Some(path) = args.spec {
        if args.uid.is_some()
            || args.gid.is_some()
            || args.cwd.is_some()
            || args.ssh_source.is_some()
            || !args.ownership_paths.is_empty()
            || !args.prepare.is_empty()
            || !args.command.is_empty()
            || args.clear_environment
        {
            return Err(InitError::InvalidCommand);
        }
        let mut spec = load_init_spec(&path)?;
        // The host deliberately supplies this at container creation time so the
        // versioned spec never needs to carry an ephemeral path.
        if spec.secret_map.is_none() {
            spec.secret_map = args.secret_map;
        }
        run_and_exec(&spec)
    } else {
        let ssh = args
            .ssh_source
            .zip(args.ssh_destination)
            .map(|(source, destination)| SshSetup {
                source,
                destination,
            });
        let mut spec = direct_spec(
            DirectInitOptions {
                uid: args.uid,
                gid: args.gid,
                cwd: args.cwd,
                secret_map: args.secret_map,
                ssh,
                prepare: args.prepare,
                allow_insecure_secret_permissions: args.allow_insecure_secret_permissions,
                clear_environment: args.clear_environment,
                ownership_paths: args.ownership_paths,
            },
            args.command,
        )?;
        spec.secret_root = args.secret_root;
        run_and_exec(&spec)
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler must install");
    tokio::select! {
        _ = interrupt => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ssh_cli_preserves_hyphenated_ssh_arguments_after_separator() {
        let cli = Cli::try_parse_from([
            "codex-start-init",
            "host-ssh",
            "--remote",
            "host.internal:22022",
            "--token-file",
            "/run/secrets/token",
            "--",
            "-p",
            "443",
            "git@github.com",
            "git-upload-pack repo",
        ])
        .unwrap();
        let Command::HostSsh(args) = cli.command else {
            panic!("expected host-ssh command");
        };
        assert_eq!(
            args.argv,
            ["-p", "443", "git@github.com", "git-upload-pack repo"]
        );
    }

    #[test]
    fn browser_open_cli_accepts_proxy_routing() {
        let cli = Cli::try_parse_from([
            "codex-start-init",
            "browser-open",
            "--bridge",
            "host.internal:4321",
            "--token-file",
            "/run/secrets/token",
            "--proxy",
            "egress:3128",
            "https://auth.example.com/oauth",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::OpenUrl(_)));
    }

    #[test]
    fn tcp_bridge_cli_accepts_authenticated_proxy_routing() {
        let cli = Cli::try_parse_from([
            "codex-start-init",
            "tcp-bridge",
            "--listen",
            "127.0.0.1:11434",
            "--remote",
            "host.internal:49152",
            "--token-file",
            "/run/codex-start/secrets/host/token",
            "--proxy",
            "egress:3128",
            "--proxy-token-file",
            "/run/codex-start/secrets/proxy/token",
        ])
        .unwrap();
        let Command::TcpBridge(args) = cli.command else {
            panic!("expected tcp-bridge command");
        };
        assert_eq!(args.proxy.as_deref(), Some("egress:3128"));
        assert_eq!(args.listen, "127.0.0.1:11434".parse().unwrap());
    }

    #[test]
    fn tcp_forward_cli_name_matches_init_service_command() {
        let cli = Cli::try_parse_from([
            "codex-start-init",
            "tcp-forward",
            "--listen",
            "127.0.0.1:1234",
            "--target",
            "host.internal:1234",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::TcpForward(_)));
    }
}
