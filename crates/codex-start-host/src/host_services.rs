//! Authenticated, per-run bridges between a workload and selected host services.
//!
//! This module deliberately separates the container-facing plan from the
//! lifetime of its host listeners. [`HostServiceManager`] owns every listener,
//! authentication token, and temporary file; [`HostServicePlan`] contains only
//! the values the container orchestrator must merge into its run/init requests.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use codex_start_core::{
    ForwardingConfig, HostServiceSpec, McpOauthCallback, NetworkMode, ProxyConfig,
};
use codex_start_proxy::{
    allowlist::{AllowList, Authority, NormalizedHost},
    auth::AuthToken,
    browser::{BrowserOpenConfig, serve_browser_open, serve_oauth_host_listener},
    connect::{TcpForwardConfig, serve_tcp_forward},
    container_init::{
        ConnectServiceSpec, InitServiceSpec, OAuthTargetServiceSpec, TcpBridgeServiceSpec,
        TcpForwardServiceSpec, UnixBridgeServiceSpec,
    },
    host_ssh::{HostSshConfig, serve_host_ssh},
    relay::{RelayConfig, RelayTarget, serve_authenticated_tcp},
};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    error::{HostError, Result},
    forwarding::ForwardingPlan,
    paths::{create_private_dir, set_private_file},
    runtime::{MountKind, MountRequest, PublishRequest, Runtime},
};

const HOST_ALIAS: &str = "codex-start-host";
const DEFAULT_EGRESS_PROXY: &str = "codex-start-proxy:3128";
const CONTAINER_TOKEN_DIRECTORY: &str = "/run/codex-start/secrets/host-services";
const CONTAINER_TOKEN_FILE: &str = "/run/codex-start/secrets/host-services/token";
const SSH_AGENT_SOCKET: &str = "/home/codex/.local/state/codex-start/ssh-agent.sock";
const GPG_AGENT_SOCKET: &str = "/home/codex/.gnupg/S.gpg-agent";
const OAUTH_TARGET_HOST: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 1_455;
const OLLAMA_PORT: u16 = 11_434;
const LM_STUDIO_PORT: u16 = 1_234;
const HOST_SSH_PROGRAM: &str = "/usr/local/bin/codex-start-host-ssh";
const INIT_PROGRAM: &str = "/usr/local/bin/codex-start-init";
const DEFAULT_SSH_RULES: &[&str] = &["github.com:22", "ssh.github.com:443"];
const DEFAULT_BROWSER_RULES: &[&str] =
    &["auth.openai.com:443", "chatgpt.com:443", "*.openai.com:443"];
const SESSION_AGENT_TARGET_ENV: &str = "CODEX_START_SESSION_AGENT_TARGET";

/// A host browser opener invoked without a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserOpener {
    /// Executable passed directly to `tokio::process::Command`.
    pub program: PathBuf,
    /// Trusted, fixed arguments inserted before the requested URL.
    pub args: Vec<OsString>,
    /// Maximum time allowed for the opener process to return.
    pub timeout_seconds: u64,
}

impl Default for BrowserOpener {
    fn default() -> Self {
        #[cfg(target_os = "macos")]
        let program = PathBuf::from("/usr/bin/open");
        #[cfg(not(target_os = "macos"))]
        let program = PathBuf::from("xdg-open");
        Self {
            program,
            args: Vec::new(),
            timeout_seconds: 10,
        }
    }
}

/// Tunable service endpoints and host executables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostServiceSettings {
    /// Container-network address of the managed egress CONNECT proxy.
    pub egress_proxy: String,
    /// Optional mounted proxy token, expressed as a container path.
    pub egress_proxy_token_file: Option<PathBuf>,
    /// Host executable and fixed arguments used for browser requests.
    pub browser_opener: BrowserOpener,
    /// Host OpenSSH executable used by the allow-listed host-SSH endpoint.
    pub ssh_program: PathBuf,
    /// Host loopback address and Codex listener port used for OAuth callbacks.
    pub oauth_callback_address: SocketAddr,
    /// In-container native Codex callback listener targeted by the relay.
    pub oauth_target_address: SocketAddr,
    /// Browser-visible native Codex callback base URL.
    pub oauth_callback_url: String,
}

impl Default for HostServiceSettings {
    fn default() -> Self {
        Self {
            egress_proxy: DEFAULT_EGRESS_PROXY.to_owned(),
            egress_proxy_token_file: None,
            browser_opener: BrowserOpener::default(),
            ssh_program: PathBuf::from("ssh"),
            oauth_callback_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                DEFAULT_OAUTH_CALLBACK_PORT,
            ),
            oauth_target_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                DEFAULT_OAUTH_CALLBACK_PORT,
            ),
            oauth_callback_url: format!("http://127.0.0.1:{DEFAULT_OAUTH_CALLBACK_PORT}"),
        }
    }
}

impl HostServiceSettings {
    /// Derives host executables and the OAuth callback from resolved forwarding
    /// configuration while retaining default internal proxy settings.
    #[must_use]
    pub fn from_forwarding(config: &ForwardingConfig) -> Self {
        let browser_opener = config.browser_opener.split_first().map_or_else(
            BrowserOpener::default,
            |(program, arguments)| BrowserOpener {
                program: PathBuf::from(program),
                args: arguments.iter().map(OsString::from).collect(),
                ..BrowserOpener::default()
            },
        );
        Self {
            browser_opener,
            ssh_program: config.host_ssh_program.clone(),
            oauth_callback_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                config.oauth_callback_port,
            ),
            oauth_target_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                config.oauth_callback_port,
            ),
            oauth_callback_url: format!("http://127.0.0.1:{}", config.oauth_callback_port),
            ..Self::default()
        }
    }

    /// Apply the callback settings resolved from native Codex configuration.
    pub fn set_oauth_callback(&mut self, callback: &McpOauthCallback) {
        self.oauth_callback_address = callback.listener();
        self.oauth_target_address = callback.codex_listener();
        callback.base_url().clone_into(&mut self.oauth_callback_url);
    }
}

/// Inputs used to prepare one run's host-service boundary.
#[derive(Debug)]
pub struct HostServiceOptions<'a> {
    /// Detected container runtime, used only for portable host-gateway names.
    pub runtime: &'a Runtime,
    /// Private directory below which ephemeral authentication state is created.
    pub runtime_parent: &'a Path,
    /// Workload network policy.
    pub network: NetworkMode,
    /// Independent host-forwarding feature switches.
    pub forwarding_config: &'a ForwardingConfig,
    /// Prepared direct mounts and fallback host Unix sockets.
    pub forwarding: &'a ForwardingPlan,
    /// Proxy connection limits applied consistently to every bridge.
    pub proxy: &'a ProxyConfig,
    /// Inherited, validated environment-declared host services.
    pub environment_services: &'a [HostServiceSpec],
    /// Exact workload argv used for byte-safe local-provider detection.
    pub workload_argv: &'a [OsString],
    /// URL authorities the host browser endpoint may open.
    pub browser_allow_hosts: &'a [String],
    /// SSH authorities the host OpenSSH endpoint may contact.
    pub allow_ssh_hosts: &'a [String],
    /// Configurable service addresses and host executable paths.
    pub settings: &'a HostServiceSettings,
}

/// Container-orchestrator additions produced by [`HostServiceManager`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostServicePlan {
    /// Long-lived loopback services launched by `codex-start-init`.
    pub init_services: Vec<InitServiceSpec>,
    /// Read-only token/config mounts required by the services.
    pub mounts: Vec<MountRequest>,
    /// Non-secret workload environment variables.
    pub env: BTreeMap<String, OsString>,
    /// Loopback-only container port publications, including OAuth reverse relay.
    pub publish: Vec<PublishRequest>,
    /// Engine host aliases required by the generated authorities.
    pub add_hosts: BTreeMap<String, String>,
    /// Authorities that an allow-list egress proxy must permit.
    pub allow_hosts: Vec<String>,
    /// Host authorities explicitly permitted to resolve privately.
    pub allow_private: Vec<String>,
    /// Container-owned directories needed for Unix bridge sockets.
    pub ownership_paths: Vec<PathBuf>,
    /// Non-fatal feature omissions and policy explanations.
    pub warnings: Vec<String>,
}

/// Local model service selected by Codex's CLI arguments.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LocalProvider {
    /// Ollama's conventional host API on port 11434.
    Ollama,
    /// LM Studio's conventional host API on port 1234.
    LmStudio,
}

/// Detects local-provider flags without converting Unix arguments to Unicode.
///
/// An explicit `--local-provider` wins over the `--oss` default. Bare `--oss`
/// selects Ollama, matching Codex's default provider.
#[must_use]
pub fn detect_local_providers(argv: &[OsString]) -> BTreeSet<LocalProvider> {
    let mut saw_oss = false;
    let mut explicit = None;
    let mut index = 0;
    while index < argv.len() {
        let argument = os_bytes(&argv[index]);
        if argument == b"--oss" {
            saw_oss = true;
        } else if argument == b"--local-provider" {
            if let Some(value) = argv.get(index + 1) {
                explicit = parse_local_provider(os_bytes(value));
                index += 1;
            }
        } else if let Some(value) = argument.strip_prefix(b"--local-provider=") {
            explicit = parse_local_provider(value);
        }
        index += 1;
    }

    explicit
        .or(saw_oss.then_some(LocalProvider::Ollama))
        .into_iter()
        .collect()
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.to_str().map_or(&[], str::as_bytes)
}

fn parse_local_provider(value: &[u8]) -> Option<LocalProvider> {
    if value.eq_ignore_ascii_case(b"ollama") {
        Some(LocalProvider::Ollama)
    } else if value.eq_ignore_ascii_case(b"lmstudio")
        || value.eq_ignore_ascii_case(b"lm-studio")
        || value.eq_ignore_ascii_case(b"lm_studio")
    {
        Some(LocalProvider::LmStudio)
    } else {
        None
    }
}

struct ServiceTask {
    label: String,
    handle: JoinHandle<std::result::Result<(), String>>,
}

/// Owns authenticated host listeners and their ephemeral authentication state.
///
/// Dropping the manager signals shutdown and aborts any task that does not stop
/// immediately. Call [`Self::shutdown`] to await orderly termination and report
/// listener failures.
pub struct HostServiceManager {
    plan: HostServicePlan,
    shutdown: watch::Sender<bool>,
    tasks: Vec<ServiceTask>,
    authentication_directory: Option<TempDir>,
    port_reservations: Vec<TcpListener>,
}

impl std::fmt::Debug for HostServiceManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostServiceManager")
            .field("plan", &self.plan)
            .field(
                "tasks",
                &self
                    .tasks
                    .iter()
                    .map(|task| &task.label)
                    .collect::<Vec<_>>(),
            )
            .field("authentication", &"[REDACTED]")
            .field(
                "authentication_directory_present",
                &self.authentication_directory.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl HostServiceManager {
    /// Binds and starts all enabled host listeners, returning only after every
    /// listener has been bound successfully.
    pub async fn start(options: HostServiceOptions<'_>) -> Result<Self> {
        validate_settings(options.settings)?;
        create_private_dir(options.runtime_parent)?;
        let (shutdown, _) = watch::channel(false);
        let mut manager = Self {
            plan: HostServicePlan::default(),
            shutdown,
            tasks: Vec::new(),
            authentication_directory: None,
            port_reservations: Vec::new(),
        };

        if options.network == NetworkMode::Offline {
            if has_requested_host_services(&options) {
                manager.plan.warnings.push(
                    "offline networking disables host agents, host SSH, browser login, OAuth callbacks, and local host services"
                        .to_owned(),
                );
            }
            return Ok(manager);
        }

        manager.plan.add_hosts = options.runtime.host_gateway_mapping();
        let authentication_required = requires_authentication(&options)?;
        let token = if authentication_required {
            let (temporary, token) = create_authentication_bundle(options.runtime_parent)?;
            manager.plan.mounts.push(MountRequest {
                kind: MountKind::Bind,
                source: Some(temporary.path().as_os_str().to_owned()),
                target: PathBuf::from(CONTAINER_TOKEN_DIRECTORY),
                read_only: true,
            });
            manager.authentication_directory = Some(temporary);
            Some(token)
        } else {
            None
        };

        manager
            .plan_network_services(&options, token.as_ref())
            .await?;

        if let Some(token) = token {
            if let Some(socket) = &options.forwarding.ssh_agent_relay {
                manager
                    .start_unix_relay(
                        "SSH agent",
                        socket,
                        PathBuf::from(SSH_AGENT_SOCKET),
                        "SSH_AUTH_SOCK",
                        token.clone(),
                        &options,
                    )
                    .await?;
            }
            if let Some(socket) = &options.forwarding.gpg_agent_relay {
                manager
                    .start_unix_relay(
                        "GPG agent",
                        socket,
                        PathBuf::from(GPG_AGENT_SOCKET),
                        "GPG_AGENT_SOCK",
                        token.clone(),
                        &options,
                    )
                    .await?;
            }
            if options.forwarding_config.host_ssh && options.network == NetworkMode::Allowlist {
                manager.start_host_ssh(token.clone(), &options).await?;
            }
            if options.forwarding_config.browser {
                manager.start_browser(token.clone(), &options).await?;
                manager.start_oauth(token, &options).await?;
            }
        }

        tokio::task::yield_now().await;
        manager.check_health().await?;
        normalize_plan(&mut manager.plan);
        Ok(manager)
    }

    /// Returns the immutable additions to merge into container orchestration.
    #[must_use]
    pub const fn plan(&self) -> &HostServicePlan {
        &self.plan
    }

    /// Release host-port guards immediately before the engine creates the
    /// workload container. Holding them through planning minimizes the
    /// otherwise unavoidable bind race in CLI-based engine adapters.
    pub fn release_port_reservations(&mut self) {
        self.port_reservations.clear();
    }

    /// Reports host listener tasks that failed since the previous health check.
    pub async fn check_health(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        let mut index = 0;
        while index < self.tasks.len() {
            if self.tasks[index].handle.is_finished() {
                let task = self.tasks.swap_remove(index);
                match task.handle.await {
                    Ok(Ok(())) if !*self.shutdown.borrow() => {
                        failures.push(format!("{} listener stopped unexpectedly", task.label));
                    }
                    Ok(Err(error)) => failures.push(format!("{}: {error}", task.label)),
                    Err(error) if !error.is_cancelled() => {
                        failures.push(format!("{} task failed: {error}", task.label));
                    }
                    _ => {}
                }
            } else {
                index += 1;
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Runtime(format!(
                "host service failure: {}",
                failures.join("; ")
            )))
        }
    }

    /// Signals every listener, awaits its exit, and reports any service error.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.shutdown.send(true);
        let tasks = std::mem::take(&mut self.tasks);
        let mut failures = Vec::new();
        for task in tasks {
            match task.handle.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("{}: {error}", task.label)),
                Err(error) => {
                    if !error.is_cancelled() {
                        failures.push(format!("{} task failed: {error}", task.label));
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Runtime(format!(
                "host service shutdown failed: {}",
                failures.join("; ")
            )))
        }
    }

    async fn plan_network_services(
        &mut self,
        options: &HostServiceOptions<'_>,
        token: Option<&AuthToken>,
    ) -> Result<()> {
        let gateway = options.runtime.host_gateway_name();
        let mut listeners = HashSet::new();
        for service in options
            .environment_services
            .iter()
            .filter(|service| !service.remove)
        {
            let (target_host, host_loopback) =
                normalize_host_service_target(&service.host, gateway)?;
            let listen_port = service.container_port.unwrap_or(service.port);
            let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen_port);
            ensure_unique_listener(&mut listeners, listen, &service.id)?;
            if host_loopback {
                self.start_tcp_relay(
                    &service.id,
                    listen,
                    service.port,
                    service.container_host.as_deref(),
                    token,
                    options,
                )
                .await?;
            } else {
                self.add_network_service(
                    listen,
                    authority(&target_host, service.port)?,
                    service.container_host.as_deref(),
                    service.allow_private,
                    options,
                )?;
            }
        }

        if options.forwarding_config.local_providers {
            for provider in detect_local_providers(options.workload_argv) {
                let (id, port) = match provider {
                    LocalProvider::Ollama => ("automatic Ollama", OLLAMA_PORT),
                    LocalProvider::LmStudio => ("automatic LM Studio", LM_STUDIO_PORT),
                };
                let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                ensure_unique_listener(&mut listeners, listen, id)?;
                self.start_tcp_relay(id, listen, port, None, token, options)
                    .await?;
                match provider {
                    LocalProvider::Ollama => {
                        self.plan.env.insert(
                            "OLLAMA_HOST".to_owned(),
                            format!("http://127.0.0.1:{OLLAMA_PORT}").into(),
                        );
                    }
                    LocalProvider::LmStudio => {
                        self.plan.env.insert(
                            "LMSTUDIO_BASE_URL".to_owned(),
                            format!("http://127.0.0.1:{LM_STUDIO_PORT}/v1").into(),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn start_tcp_relay(
        &mut self,
        label: &str,
        listen: SocketAddr,
        target_port: u16,
        container_host: Option<&str>,
        token: Option<&AuthToken>,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        self.add_container_alias(container_host)?;
        if options.network == NetworkMode::Host {
            if listen.port() != target_port {
                return Err(HostError::Config(format!(
                    "host networking cannot remap host loopback port {target_port} to {}",
                    listen.port()
                )));
            }
            return Ok(());
        }
        let token = required_relay_token(token, label)?;

        let host_listener = bind_authenticated_listener(label).await?;
        let remote = host_remote(
            options.runtime,
            host_listener.local_addr().map_err(|source| {
                HostError::Runtime(format!("failed to inspect {label} relay: {source}"))
            })?,
        )?;
        self.permit_authenticated_remote(&remote);
        let limits = service_limits(options.proxy);
        self.plan
            .init_services
            .push(InitServiceSpec::TcpBridge(TcpBridgeServiceSpec {
                listen,
                remote,
                token_file: PathBuf::from(CONTAINER_TOKEN_FILE),
                proxy: proxy_for(options.network, options.settings),
                proxy_auth_token_file: proxy_token_for(options.network, options.settings),
                max_connections: limits.max_connections,
                connect_timeout_seconds: options.proxy.connect_timeout_seconds,
                handshake_timeout_seconds: options.proxy.handshake_timeout_seconds,
                idle_timeout_seconds: options.proxy.idle_timeout_seconds,
            }));

        let target = RelayTarget::Tcp(format!("127.0.0.1:{target_port}"));
        let mut shutdown = self.shutdown.subscribe();
        let task_label = label.to_owned();
        self.tasks.push(ServiceTask {
            label: task_label,
            handle: tokio::spawn(async move {
                serve_authenticated_tcp(host_listener, target, token, limits, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    fn add_network_service(
        &mut self,
        listen: SocketAddr,
        target: String,
        container_host: Option<&str>,
        allow_private: bool,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        self.add_container_alias(container_host)?;
        if options.network == NetworkMode::Host
            && authority_uses_host(&target, options.runtime.host_gateway_name())?
        {
            let target_authority = Authority::parse(&target, None).map_err(|error| {
                HostError::Config(format!("invalid generated host service target: {error}"))
            })?;
            if listen.port() != target_authority.port {
                return Err(HostError::Config(format!(
                    "host networking cannot remap host service {target} to loopback port {}",
                    listen.port()
                )));
            }
            return Ok(());
        }
        self.plan.allow_hosts.push(target.clone());
        if allow_private {
            self.plan.allow_private.push(target.clone());
        } else if options.network == NetworkMode::Allowlist
            && options.proxy.block_private_addresses
            && authority_uses_host(&target, options.runtime.host_gateway_name())?
        {
            self.plan.warnings.push(format!(
                "host service {target} is not marked allow_private and may be denied by private-address policy"
            ));
        }
        let service = network_service_spec(
            options.network,
            listen,
            target,
            options.proxy,
            options.settings,
        )?;
        self.plan.init_services.push(service);
        Ok(())
    }

    fn add_container_alias(&mut self, container_host: Option<&str>) -> Result<()> {
        if let Some(alias) = container_host {
            validate_container_alias(alias)?;
            if alias != "localhost" {
                self.plan
                    .add_hosts
                    .insert(alias.to_owned(), Ipv4Addr::LOCALHOST.to_string());
            }
        }
        Ok(())
    }

    async fn start_unix_relay(
        &mut self,
        label: &str,
        socket: &Path,
        container_socket: PathBuf,
        env_name: &str,
        token: AuthToken,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        let listener = bind_authenticated_listener(label).await?;
        let remote = host_remote(
            options.runtime,
            listener.local_addr().map_err(|source| {
                HostError::Runtime(format!("failed to inspect {label} relay: {source}"))
            })?,
        )?;
        self.permit_authenticated_remote(&remote);
        let limits = service_limits(options.proxy);
        self.plan
            .init_services
            .push(InitServiceSpec::UnixBridge(UnixBridgeServiceSpec {
                listen: container_socket.clone(),
                remote,
                token_file: PathBuf::from(CONTAINER_TOKEN_FILE),
                proxy: proxy_for(options.network, options.settings),
                proxy_auth_token_file: proxy_token_for(options.network, options.settings),
                max_connections: limits.max_connections,
                connect_timeout_seconds: options.proxy.connect_timeout_seconds,
                handshake_timeout_seconds: options.proxy.handshake_timeout_seconds,
                idle_timeout_seconds: options.proxy.idle_timeout_seconds,
            }));
        self.plan
            .env
            .insert(env_name.to_owned(), container_socket.as_os_str().to_owned());
        let ownership = container_socket.parent().map(Path::to_path_buf);
        if let Some(path) = ownership
            && !self.plan.ownership_paths.contains(&path)
        {
            self.plan.ownership_paths.push(path);
        }

        let target = if label == "SSH agent" {
            std::env::var_os(SESSION_AGENT_TARGET_ENV).map_or_else(
                || RelayTarget::Unix(socket.to_owned()),
                |path| RelayTarget::UnixTargetFile(PathBuf::from(path)),
            )
        } else {
            RelayTarget::Unix(socket.to_owned())
        };
        let mut shutdown = self.shutdown.subscribe();
        let config = limits;
        let task_label = label.to_owned();
        self.tasks.push(ServiceTask {
            label: task_label,
            handle: tokio::spawn(async move {
                serve_authenticated_tcp(listener, target, token, config, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    async fn start_host_ssh(
        &mut self,
        token: AuthToken,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        let rules = if options.allow_ssh_hosts.is_empty() {
            DEFAULT_SSH_RULES.iter().map(ToString::to_string).collect()
        } else {
            normalize_ssh_rules(options.allow_ssh_hosts)?
        };
        let allowlist = parse_allowlist(&rules, "SSH")?;
        let listener = bind_authenticated_listener("host SSH").await?;
        let remote = host_remote(
            options.runtime,
            listener.local_addr().map_err(|source| {
                HostError::Runtime(format!("failed to inspect host SSH relay: {source}"))
            })?,
        )?;
        self.permit_authenticated_remote(&remote);
        let limits = service_limits(options.proxy);
        self.plan.env.insert(
            "GIT_SSH_COMMAND".to_owned(),
            OsString::from(HOST_SSH_PROGRAM),
        );
        self.plan
            .env
            .insert("GIT_SSH_VARIANT".to_owned(), OsString::from("ssh"));
        self.plan.env.insert(
            "CODEX_START_HOST_SSH_ADDR".to_owned(),
            OsString::from(&remote),
        );
        self.plan.env.insert(
            "CODEX_START_HOST_SSH_TOKEN_FILE".to_owned(),
            OsString::from(CONTAINER_TOKEN_FILE),
        );
        for (name, value) in [
            (
                "CODEX_START_HOST_SSH_CONNECT_TIMEOUT",
                options.proxy.connect_timeout_seconds,
            ),
            (
                "CODEX_START_HOST_SSH_HANDSHAKE_TIMEOUT",
                options.proxy.handshake_timeout_seconds,
            ),
            (
                "CODEX_START_HOST_SSH_IDLE_TIMEOUT",
                options.proxy.idle_timeout_seconds,
            ),
        ] {
            self.plan
                .env
                .insert(name.to_owned(), OsString::from(value.to_string()));
        }
        if let Some(proxy) = proxy_for(options.network, options.settings) {
            self.plan.env.insert(
                "CODEX_START_HOST_SSH_PROXY".to_owned(),
                OsString::from(proxy),
            );
        }
        if let Some(path) = proxy_token_for(options.network, options.settings) {
            self.plan.env.insert(
                "CODEX_START_HOST_SSH_PROXY_TOKEN_FILE".to_owned(),
                path.into_os_string(),
            );
        }

        let config = HostSshConfig {
            allowlist,
            ssh_program: options.settings.ssh_program.clone(),
            relay: limits,
        };
        let mut shutdown = self.shutdown.subscribe();
        self.tasks.push(ServiceTask {
            label: "host SSH".to_owned(),
            handle: tokio::spawn(async move {
                serve_host_ssh(listener, token, config, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    async fn start_browser(
        &mut self,
        token: AuthToken,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        let mut rules = options.browser_allow_hosts.to_vec();
        for rule in DEFAULT_BROWSER_RULES {
            if !rules.iter().any(|existing| existing == rule) {
                rules.push((*rule).to_owned());
            }
        }
        let allowlist = parse_allowlist(&rules, "browser")?;
        let listener = bind_authenticated_listener("browser opener").await?;
        let remote = host_remote(
            options.runtime,
            listener.local_addr().map_err(|source| {
                HostError::Runtime(format!("failed to inspect browser relay: {source}"))
            })?,
        )?;
        self.permit_authenticated_remote(&remote);
        let limits = service_limits(options.proxy);
        let browser = browser_command(&remote, options.network, options.settings, options.proxy);
        self.plan
            .env
            .insert("BROWSER".to_owned(), OsString::from(browser));
        self.plan.env.insert(
            "CODEX_START_BROWSER_ADDR".to_owned(),
            OsString::from(&remote),
        );
        self.plan.env.insert(
            "CODEX_START_BROWSER_TOKEN_FILE".to_owned(),
            OsString::from(CONTAINER_TOKEN_FILE),
        );

        let config = BrowserOpenConfig {
            allowlist,
            opener_program: options.settings.browser_opener.program.clone(),
            opener_args: options.settings.browser_opener.args.clone(),
            relay: limits,
            opener_timeout: Duration::from_secs(options.settings.browser_opener.timeout_seconds),
        };
        let mut shutdown = self.shutdown.subscribe();
        self.tasks.push(ServiceTask {
            label: "browser opener".to_owned(),
            handle: tokio::spawn(async move {
                serve_browser_open(listener, token, config, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    async fn start_oauth(
        &mut self,
        token: AuthToken,
        options: &HostServiceOptions<'_>,
    ) -> Result<()> {
        if options.network == NetworkMode::Host {
            return self.start_host_network_oauth(options).await;
        }
        let callback = options.settings.oauth_callback_address;
        let listener = match TcpListener::bind(callback).await {
            Ok(listener) => listener,
            Err(source) if source.kind() == std::io::ErrorKind::AddrInUse => {
                self.plan.warnings.push(format!(
                    "Codex OAuth callback {callback} is already in use; browser opening remains available but this run cannot own the reverse callback"
                ));
                return Ok(());
            }
            Err(source) => {
                return Err(HostError::Runtime(format!(
                    "cannot bind Codex OAuth callback {callback}: {source}"
                )));
            }
        };
        let host_reservation = reserve_ephemeral_port().await?;
        let host_port = host_reservation
            .local_addr()
            .map_err(|source| {
                HostError::Runtime(format!("failed to inspect reserved port: {source}"))
            })?
            .port();
        let container_reservation = reserve_ephemeral_port().await?;
        let container_port = container_reservation
            .local_addr()
            .map_err(|source| {
                HostError::Runtime(format!("failed to inspect reserved port: {source}"))
            })?
            .port();
        drop(container_reservation);
        self.port_reservations.push(host_reservation);
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port).to_string();
        let target_listen = SocketAddr::new(IpAddr::V4(OAUTH_TARGET_HOST), container_port);
        let limits = service_limits(options.proxy);
        self.plan
            .init_services
            .push(InitServiceSpec::OauthTarget(OAuthTargetServiceSpec {
                listen: target_listen,
                callback: options.settings.oauth_target_address,
                token_file: PathBuf::from(CONTAINER_TOKEN_FILE),
                max_connections: limits.max_connections,
                connect_timeout_seconds: options.proxy.connect_timeout_seconds,
                handshake_timeout_seconds: options.proxy.handshake_timeout_seconds,
                idle_timeout_seconds: options.proxy.idle_timeout_seconds,
            }));
        self.plan.publish.push(PublishRequest {
            host_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            host_port,
            container_port,
            protocol: "tcp".to_owned(),
        });
        self.plan.env.insert(
            "CODEX_START_OAUTH_CALLBACK".to_owned(),
            OsString::from(&options.settings.oauth_callback_url),
        );

        let mut shutdown = self.shutdown.subscribe();
        self.tasks.push(ServiceTask {
            label: "OAuth callback".to_owned(),
            handle: tokio::spawn(async move {
                serve_oauth_host_listener(listener, remote, token, limits, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    async fn start_host_network_oauth(&mut self, options: &HostServiceOptions<'_>) -> Result<()> {
        let callback = options.settings.oauth_callback_address;
        let target = options.settings.oauth_target_address;
        self.plan.env.insert(
            "CODEX_START_OAUTH_CALLBACK".to_owned(),
            OsString::from(&options.settings.oauth_callback_url),
        );
        if callback == target {
            return Ok(());
        }
        let listener = match TcpListener::bind(callback).await {
            Ok(listener) => listener,
            Err(source) if source.kind() == std::io::ErrorKind::AddrInUse => {
                self.plan.warnings.push(format!(
                    "Codex OAuth callback {callback} is already in use; browser opening remains available but this run cannot own the callback relay"
                ));
                return Ok(());
            }
            Err(source) => {
                return Err(HostError::Runtime(format!(
                    "cannot bind Codex OAuth callback {callback}: {source}"
                )));
            }
        };
        let target = Authority::parse(&target.to_string(), None).map_err(|error| {
            HostError::Config(format!(
                "invalid native Codex OAuth callback target: {error}"
            ))
        })?;
        let config = TcpForwardConfig {
            target,
            relay: service_limits(options.proxy),
        };
        let mut shutdown = self.shutdown.subscribe();
        self.tasks.push(ServiceTask {
            label: "OAuth callback".to_owned(),
            handle: tokio::spawn(async move {
                serve_tcp_forward(listener, config, async move {
                    wait_for_shutdown(&mut shutdown).await;
                })
                .await
                .map_err(|error| error.to_string())
            }),
        });
        Ok(())
    }

    fn permit_authenticated_remote(&mut self, remote: &str) {
        self.plan.allow_hosts.push(remote.to_owned());
        self.plan.allow_private.push(remote.to_owned());
    }
}

impl Drop for HostServiceManager {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.handle.abort();
        }
    }
}

fn validate_settings(settings: &HostServiceSettings) -> Result<()> {
    Authority::parse(&settings.egress_proxy, None).map_err(|error| {
        HostError::Config(format!(
            "invalid host-service egress proxy {}: {error}",
            settings.egress_proxy
        ))
    })?;
    if !valid_oauth_callback_settings(settings)
        || settings.browser_opener.timeout_seconds == 0
        || settings.browser_opener.program.as_os_str().is_empty()
        || settings.ssh_program.as_os_str().is_empty()
    {
        return Err(HostError::Config(
            "host-service ports, timeouts, and executable paths must be non-empty/non-zero"
                .to_owned(),
        ));
    }
    if !safe_process_value(settings.browser_opener.program.as_os_str())
        || settings
            .browser_opener
            .args
            .iter()
            .any(|argument| !safe_process_value(argument))
        || !safe_process_value(settings.ssh_program.as_os_str())
    {
        return Err(HostError::Config(
            "host-service executable paths and arguments cannot contain NUL bytes".to_owned(),
        ));
    }
    if settings
        .egress_proxy_token_file
        .as_deref()
        .is_some_and(|path| !safe_command_word(path.as_os_str()))
    {
        return Err(HostError::Config(
            "egress proxy token path contains characters unsafe for the fixed BROWSER command"
                .to_owned(),
        ));
    }
    Ok(())
}

fn valid_oauth_callback_settings(settings: &HostServiceSettings) -> bool {
    let address = settings.oauth_callback_address;
    let target = settings.oauth_target_address;
    if !address.ip().is_loopback()
        || address.port() == 0
        || target.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST)
        || target.port() != address.port()
    {
        return false;
    }
    let Ok(url) = url::Url::parse(&settings.oauth_callback_url) else {
        return false;
    };
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(address.port())
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost") && address.ip().is_ipv4()
        }
        Some(url::Host::Ipv4(ip)) => address.ip() == IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => address.ip() == IpAddr::V6(ip),
        None => false,
    }
}

fn has_requested_host_services(options: &HostServiceOptions<'_>) -> bool {
    options.forwarding.ssh_agent_relay.is_some()
        || options.forwarding.gpg_agent_relay.is_some()
        || options.forwarding_config.host_ssh
        || options.forwarding_config.browser
        || !options.environment_services.is_empty()
        || options.forwarding_config.local_providers
            && !detect_local_providers(options.workload_argv).is_empty()
}

fn requires_authentication(options: &HostServiceOptions<'_>) -> Result<bool> {
    if options.network == NetworkMode::Offline {
        return Ok(false);
    }
    if options.forwarding.ssh_agent_relay.is_some()
        || options.forwarding.gpg_agent_relay.is_some()
        || (options.forwarding_config.host_ssh && options.network == NetworkMode::Allowlist)
        || options.forwarding_config.browser
    {
        return Ok(true);
    }
    if options.network == NetworkMode::Host {
        return Ok(false);
    }
    if options.forwarding_config.local_providers
        && !detect_local_providers(options.workload_argv).is_empty()
    {
        return Ok(true);
    }
    options
        .environment_services
        .iter()
        .filter(|service| !service.remove)
        .try_fold(false, |required, service| {
            normalize_host_service_target(&service.host, options.runtime.host_gateway_name())
                .map(|(_, host_loopback)| required || host_loopback)
        })
}

fn required_relay_token(token: Option<&AuthToken>, label: &str) -> Result<AuthToken> {
    token.cloned().ok_or_else(|| {
        HostError::Runtime(format!(
            "internal error: authenticated host relay {label} has no token"
        ))
    })
}

fn normalize_host_service_target(host: &str, gateway: &str) -> Result<(String, bool)> {
    let requested = NormalizedHost::parse(host).map_err(|error| {
        HostError::Config(format!("invalid host service target {host}: {error}"))
    })?;
    let gateway_host = NormalizedHost::parse(gateway).map_err(|error| {
        HostError::Config(format!("invalid runtime host gateway {gateway}: {error}"))
    })?;
    let is_host_loopback = matches!(
        host,
        "host.containers.internal" | "host.docker.internal" | HOST_ALIAS
    ) || requested == NormalizedHost::Domain("localhost".to_owned())
        || matches!(requested, NormalizedHost::Ip(ip) if ip.is_loopback());
    if is_host_loopback {
        Ok((gateway_host.authority_name(), true))
    } else {
        Ok((requested.authority_name(), false))
    }
}

fn authority(host: &str, port: u16) -> Result<String> {
    if port == 0 {
        return Err(HostError::Config(
            "host service ports must be non-zero".to_owned(),
        ));
    }
    let host = NormalizedHost::parse(host).map_err(|error| {
        HostError::Config(format!("invalid host service target {host}: {error}"))
    })?;
    Ok(format!("{}:{port}", host.authority_name()))
}

fn validate_container_alias(alias: &str) -> Result<()> {
    match NormalizedHost::parse(alias) {
        Ok(NormalizedHost::Domain(_)) => Ok(()),
        Ok(NormalizedHost::Ip(IpAddr::V4(ip))) if ip == Ipv4Addr::LOCALHOST => Ok(()),
        Ok(NormalizedHost::Ip(_)) => Err(HostError::Config(format!(
            "container_host {alias} must be a hostname or loopback address"
        ))),
        Err(error) => Err(HostError::Config(format!(
            "invalid container_host {alias}: {error}"
        ))),
    }
}

fn authority_uses_host(value: &str, expected: &str) -> Result<bool> {
    let authority = Authority::parse(value, None).map_err(|error| {
        HostError::Config(format!("invalid generated authority {value}: {error}"))
    })?;
    let expected = NormalizedHost::parse(expected).map_err(|error| {
        HostError::Config(format!(
            "invalid container-runtime host gateway {expected}: {error}"
        ))
    })?;
    Ok(authority.host == expected)
}

fn ensure_unique_listener(
    listeners: &mut HashSet<SocketAddr>,
    listen: SocketAddr,
    id: &str,
) -> Result<()> {
    if listeners.insert(listen) {
        Ok(())
    } else {
        Err(HostError::Config(format!(
            "host service {id} duplicates container listener {listen}"
        )))
    }
}

fn parse_allowlist(rules: &[String], label: &str) -> Result<AllowList> {
    AllowList::parse(rules.iter().map(String::as_str))
        .map_err(|error| HostError::Config(format!("invalid {label} host allow-list: {error}")))
}

fn normalize_ssh_rules(rules: &[String]) -> Result<Vec<String>> {
    rules
        .iter()
        .map(|rule| {
            if rule.starts_with("*.") {
                if rule.contains(':') {
                    Ok(rule.clone())
                } else {
                    Ok(format!("{rule}:22"))
                }
            } else {
                Authority::parse(rule, Some(22))
                    .map(|authority| authority.display_with_port())
                    .map_err(|error| {
                        HostError::Config(format!("invalid SSH host rule {rule}: {error}"))
                    })
            }
        })
        .collect()
}

fn service_limits(proxy: &ProxyConfig) -> RelayConfig {
    RelayConfig {
        max_connections: proxy.max_connections,
        connect_timeout: Duration::from_secs(proxy.connect_timeout_seconds),
        handshake_timeout: Duration::from_secs(proxy.handshake_timeout_seconds),
        idle_timeout: Duration::from_secs(proxy.idle_timeout_seconds),
    }
}

fn network_service_spec(
    network: NetworkMode,
    listen: SocketAddr,
    target: String,
    proxy: &ProxyConfig,
    settings: &HostServiceSettings,
) -> Result<InitServiceSpec> {
    match network {
        NetworkMode::Allowlist => Ok(InitServiceSpec::Connect(ConnectServiceSpec {
            listen,
            proxy: settings.egress_proxy.clone(),
            target,
            auth_token_file: settings.egress_proxy_token_file.clone(),
            max_connections: proxy.max_connections,
            connect_timeout_seconds: proxy.connect_timeout_seconds,
            handshake_timeout_seconds: proxy.handshake_timeout_seconds,
            idle_timeout_seconds: proxy.idle_timeout_seconds,
        })),
        NetworkMode::Bridge | NetworkMode::Host => {
            Ok(InitServiceSpec::TcpForward(TcpForwardServiceSpec {
                listen,
                target,
                max_connections: proxy.max_connections,
                connect_timeout_seconds: proxy.connect_timeout_seconds,
                idle_timeout_seconds: proxy.idle_timeout_seconds,
            }))
        }
        NetworkMode::Offline => Err(HostError::Config(
            "internal error: attempted to plan a host service in offline mode".to_owned(),
        )),
    }
}

fn proxy_for(network: NetworkMode, settings: &HostServiceSettings) -> Option<String> {
    (network == NetworkMode::Allowlist).then(|| settings.egress_proxy.clone())
}

fn proxy_token_for(network: NetworkMode, settings: &HostServiceSettings) -> Option<PathBuf> {
    (network == NetworkMode::Allowlist)
        .then(|| settings.egress_proxy_token_file.clone())
        .flatten()
}

fn browser_command(
    remote: &str,
    network: NetworkMode,
    settings: &HostServiceSettings,
    proxy: &ProxyConfig,
) -> String {
    let mut command = format!(
        "{INIT_PROGRAM} browser-open --bridge {remote} --token-file {CONTAINER_TOKEN_FILE}"
    );
    if network == NetworkMode::Allowlist {
        command.push_str(" --proxy ");
        command.push_str(&settings.egress_proxy);
        if let Some(path) = &settings.egress_proxy_token_file {
            command.push_str(" --proxy-token-file ");
            command.push_str(path.to_string_lossy().as_ref());
        }
    }
    command.push_str(" --max-connections ");
    command.push_str(&proxy.max_connections.to_string());
    command.push_str(" --connect-timeout-seconds ");
    command.push_str(&proxy.connect_timeout_seconds.to_string());
    command.push_str(" --handshake-timeout-seconds ");
    command.push_str(&proxy.handshake_timeout_seconds.to_string());
    command.push_str(" --idle-timeout-seconds ");
    command.push_str(&proxy.idle_timeout_seconds.to_string());
    command
}

fn safe_command_word(value: &OsStr) -> bool {
    !os_bytes(value).is_empty()
        && os_bytes(value).iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':')
        })
}

fn safe_process_value(value: &OsStr) -> bool {
    !os_bytes(value).contains(&0)
}

async fn bind_authenticated_listener(label: &str) -> Result<TcpListener> {
    // Wildcard binding is required on native Linux because the engine reaches
    // the listener through its bridge gateway rather than host loopback. Every
    // protocol using this helper authenticates before touching its target.
    TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .map_err(|source| HostError::Runtime(format!("failed to bind {label} relay: {source}")))
}

fn host_remote(runtime: &Runtime, address: SocketAddr) -> Result<String> {
    authority(runtime.host_gateway_name(), address.port())
}

async fn reserve_ephemeral_port() -> Result<TcpListener> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|source| {
            HostError::Runtime(format!("failed to reserve a loopback port: {source}"))
        })?;
    Ok(listener)
}

fn create_authentication_bundle(runtime_parent: &Path) -> Result<(TempDir, AuthToken)> {
    let directory = tempfile::Builder::new()
        .prefix("host-services-")
        .tempdir_in(runtime_parent)
        .map_err(|source| HostError::io(runtime_parent, source))?;
    create_private_dir(directory.path())?;
    let token_path = directory.path().join("token");
    let mut token_bytes = Vec::with_capacity(32);
    token_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    token_bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let token = AuthToken::new(token_bytes.clone())
        .map_err(|error| HostError::Runtime(format!("could not create relay token: {error}")))?;
    let write_result = write_private_file(&token_path, &token_bytes);
    token_bytes.zeroize();
    write_result?;
    Ok((directory, token))
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|source| HostError::io(path, source))?;
    file.write_all(contents)
        .map_err(|source| HostError::io(path, source))?;
    file.sync_all()
        .map_err(|source| HostError::io(path, source))?;
    set_private_file(path)
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

fn normalize_plan(plan: &mut HostServicePlan) {
    plan.allow_hosts.sort();
    plan.allow_hosts.dedup();
    plan.allow_private.sort();
    plan.allow_private.dedup();
    plan.ownership_paths.sort();
    plan.ownership_paths.dedup();
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        fs,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::{Path, PathBuf},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use codex_start_core::{
        CodexConfig, ForwardingConfig, HostServiceSpec, NetworkMode, ProxyConfig, SshAgentBridge,
    };
    use codex_start_proxy::container_init::InitServiceSpec;

    use super::{
        CONTAINER_TOKEN_FILE, HostServiceManager, HostServiceOptions, HostServicePlan,
        HostServiceSettings, LocalProvider, create_authentication_bundle, detect_local_providers,
        ensure_unique_listener, network_service_spec, normalize_host_service_target,
        normalize_ssh_rules,
    };
    use crate::{
        forwarding::{ForwardingOptions, ForwardingPlan},
        runtime::{Runtime, RuntimeKind},
    };

    struct Harness {
        _root: tempfile::TempDir,
        runtime: Runtime,
        forwarding: ForwardingPlan,
        runtime_parent: PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let runtime_parent = root.path().join("runtime");
            let runtime = fake_runtime(root.path());
            let forwarding = ForwardingPlan::prepare(
                &runtime,
                &ForwardingOptions {
                    ssh_agent: false,
                    ssh_agent_bridge: SshAgentBridge::Auto,
                    gpg_agent: false,
                    git_config: false,
                    known_hosts: false,
                    gh_config: false,
                    ..ForwardingOptions::default()
                },
                &runtime_parent,
            )
            .unwrap();
            Self {
                _root: root,
                runtime,
                forwarding,
                runtime_parent,
            }
        }
    }

    struct ManagerInputs<'a> {
        network: NetworkMode,
        config: &'a ForwardingConfig,
        services: &'a [HostServiceSpec],
        argv: &'a [OsString],
        settings: &'a HostServiceSettings,
    }

    async fn start_manager(harness: &Harness, inputs: &ManagerInputs<'_>) -> HostServiceManager {
        HostServiceManager::start(HostServiceOptions {
            runtime: &harness.runtime,
            runtime_parent: &harness.runtime_parent,
            network: inputs.network,
            forwarding_config: inputs.config,
            forwarding: &harness.forwarding,
            proxy: &ProxyConfig::default(),
            environment_services: inputs.services,
            workload_argv: inputs.argv,
            browser_allow_hosts: &[],
            allow_ssh_hosts: &[],
            settings: inputs.settings,
        })
        .await
        .unwrap()
    }

    fn disabled_forwarding() -> ForwardingConfig {
        ForwardingConfig {
            ssh_agent: false,
            gpg_agent: false,
            git_config: false,
            known_hosts: false,
            host_ssh: false,
            gh_config: false,
            browser: false,
            local_providers: false,
            ..ForwardingConfig::default()
        }
    }

    fn fake_runtime(root: &Path) -> Runtime {
        let executable = root.join("fake-docker");
        fs::write(
            &executable,
            b"#!/bin/sh\nif [ \"$1\" = version ]; then echo 29.0.0; exit 0; fi\ncase \"$*\" in *--help*) echo --add-host --cap-add --cap-drop --label --mount --network --network-alias --read-only --security-opt --userns --internal --alias --filter --format;; esac\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Runtime::detect(RuntimeKind::Docker, Some(executable.as_os_str())).unwrap()
    }

    fn mounted_authentication_directory(plan: &HostServicePlan) -> Option<PathBuf> {
        plan.mounts
            .iter()
            .find(|mount| mount.target == Path::new("/run/codex-start/secrets/host-services"))
            .and_then(|mount| mount.source.as_deref())
            .map(PathBuf::from)
    }

    fn host_service(
        host: &str,
        port: u16,
        container_host: Option<&str>,
        container_port: Option<u16>,
        allow_private: bool,
    ) -> HostServiceSpec {
        HostServiceSpec {
            id: "test-service".to_owned(),
            host: host.to_owned(),
            port,
            container_host: container_host.map(str::to_owned),
            container_port,
            allow_private,
            remove: false,
        }
    }

    #[test]
    fn detects_provider_flags_without_unicode_round_trip() {
        assert_eq!(
            detect_local_providers(&["codex".into(), "--oss".into()]),
            [LocalProvider::Ollama].into_iter().collect()
        );
        assert_eq!(
            detect_local_providers(&[
                "codex".into(),
                "--oss".into(),
                "--local-provider=lmstudio".into(),
            ]),
            [LocalProvider::LmStudio].into_iter().collect()
        );
        assert_eq!(
            detect_local_providers(&["codex".into(), "--local-provider".into(), "ollama".into(),]),
            [LocalProvider::Ollama].into_iter().collect()
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid = OsString::from_vec(b"--local-provider=ollama\xff".to_vec());
            assert!(detect_local_providers(&[invalid]).is_empty());
        }
    }

    #[test]
    fn host_service_target_uses_runtime_gateway_portably() {
        assert_eq!(
            normalize_host_service_target("host.containers.internal", "host.docker.internal")
                .unwrap(),
            ("host.docker.internal".to_owned(), true)
        );
        assert_eq!(
            normalize_host_service_target("models.example.test", "ignored").unwrap(),
            ("models.example.test".to_owned(), false)
        );
        assert_eq!(
            normalize_host_service_target("127.0.0.1", "host.docker.internal").unwrap(),
            ("host.docker.internal".to_owned(), true)
        );
    }

    #[test]
    fn resolved_forwarding_config_drives_host_executables() {
        let config = ForwardingConfig {
            browser_opener: vec!["custom-open".to_owned(), "--background".to_owned()],
            host_ssh_program: "/usr/bin/ssh".into(),
            oauth_callback_port: 9_999,
            ..ForwardingConfig::default()
        };
        let settings = HostServiceSettings::from_forwarding(&config);
        assert_eq!(
            settings.browser_opener.program,
            std::path::Path::new("custom-open")
        );
        assert_eq!(settings.browser_opener.args, ["--background"]);
        assert_eq!(settings.ssh_program, std::path::Path::new("/usr/bin/ssh"));
        assert_eq!(
            settings.oauth_callback_address,
            "127.0.0.1:9999".parse().unwrap()
        );
        assert_eq!(settings.oauth_callback_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn ssh_rules_default_to_port_twenty_two() {
        assert_eq!(
            normalize_ssh_rules(&["git.example.test".to_owned()]).unwrap(),
            ["git.example.test:22"]
        );
        assert_eq!(
            normalize_ssh_rules(&["*.example.test".to_owned()]).unwrap(),
            ["*.example.test:22"]
        );
    }

    #[test]
    fn planning_rejects_duplicate_loopback_listeners() {
        let mut listeners = std::collections::HashSet::new();
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 11_434);
        ensure_unique_listener(&mut listeners, address, "ollama").unwrap();
        assert!(ensure_unique_listener(&mut listeners, address, "duplicate").is_err());
    }

    #[test]
    fn public_plan_is_mergeable_and_defaults_empty() {
        let plan = HostServicePlan::default();
        assert!(plan.init_services.is_empty());
        assert!(plan.mounts.is_empty());
        assert!(plan.env.is_empty());
        assert!(plan.publish.is_empty());
        assert!(plan.allow_hosts.is_empty());
        assert!(plan.allow_private.is_empty());
    }

    #[test]
    fn generated_authentication_file_is_private_and_redacted() {
        let parent = tempfile::tempdir().unwrap();
        let (directory, token) = create_authentication_bundle(parent.path()).unwrap();
        let path = directory.path().join("token");
        assert!(token.matches(&std::fs::read(&path).unwrap()));
        assert_eq!(format!("{token:?}"), "AuthToken([REDACTED])");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn planning_selects_connect_or_direct_transport_from_network_policy() {
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 11_434);
        let proxy = ProxyConfig::default();
        let settings = HostServiceSettings::default();
        let allowlist = network_service_spec(
            NetworkMode::Allowlist,
            listen,
            "codex-start-host:11434".to_owned(),
            &proxy,
            &settings,
        )
        .unwrap();
        let bridge = network_service_spec(
            NetworkMode::Bridge,
            listen,
            "codex-start-host:11434".to_owned(),
            &proxy,
            &settings,
        )
        .unwrap();
        assert!(matches!(allowlist, InitServiceSpec::Connect(_)));
        assert!(matches!(bridge, InitServiceSpec::TcpForward(_)));
        assert!(
            network_service_spec(
                NetworkMode::Offline,
                listen,
                "codex-start-host:11434".to_owned(),
                &proxy,
                &settings,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn offline_mode_creates_no_host_reachability_or_authentication_state() {
        let harness = Harness::new();
        let config = ForwardingConfig::default();
        let settings = HostServiceSettings::from_forwarding(&config);
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Offline,
                config: &config,
                services: &[host_service(
                    "host.containers.internal",
                    11_434,
                    None,
                    None,
                    true,
                )],
                argv: &["codex".into(), "--oss".into()],
                settings: &settings,
            },
        )
        .await;
        let plan = manager.plan();
        assert!(plan.init_services.is_empty());
        assert!(plan.mounts.is_empty());
        assert!(plan.env.is_empty());
        assert!(plan.publish.is_empty());
        assert!(plan.add_hosts.is_empty());
        assert!(plan.allow_hosts.is_empty());
        assert!(plan.allow_private.is_empty());
        assert_eq!(plan.warnings.len(), 1);
        assert!(manager.tasks.is_empty());
        assert!(manager.authentication_directory.is_none());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn allowlist_local_provider_uses_authenticated_bridge_and_cleans_token() {
        let harness = Harness::new();
        let config = ForwardingConfig {
            local_providers: true,
            ..disabled_forwarding()
        };
        let settings = HostServiceSettings::from_forwarding(&config);
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Allowlist,
                config: &config,
                services: &[],
                argv: &["codex".into(), "--oss".into()],
                settings: &settings,
            },
        )
        .await;
        let plan = manager.plan();
        let bridge = plan
            .init_services
            .iter()
            .find_map(|service| match service {
                InitServiceSpec::TcpBridge(service) => Some(service),
                _ => None,
            })
            .unwrap();
        assert_eq!(bridge.listen, "127.0.0.1:11434".parse().unwrap());
        assert_eq!(bridge.proxy.as_deref(), Some("codex-start-proxy:3128"));
        assert_eq!(bridge.token_file, Path::new(CONTAINER_TOKEN_FILE));
        assert_eq!(
            plan.env.get("OLLAMA_HOST").map(OsString::as_os_str),
            Some(OsStr::new("http://127.0.0.1:11434"))
        );
        assert!(plan.allow_hosts.contains(&bridge.remote));
        assert!(plan.allow_private.contains(&bridge.remote));
        assert_eq!(
            plan.add_hosts
                .get("host.docker.internal")
                .map(String::as_str),
            Some("host-gateway")
        );
        assert_eq!(manager.tasks.len(), 1);

        let authentication_directory = mounted_authentication_directory(plan).unwrap();
        let token_bytes = fs::read(authentication_directory.join("token")).unwrap();
        let encoded_token = BASE64.encode(token_bytes);
        assert!(!format!("{manager:?}").contains(&encoded_token));
        manager.shutdown().await.unwrap();
        assert!(!authentication_directory.exists());
    }

    #[tokio::test]
    async fn bridge_provider_omits_proxy_while_host_mode_needs_no_bridge_or_token() {
        let config = ForwardingConfig {
            local_providers: true,
            ..disabled_forwarding()
        };
        let settings = HostServiceSettings::from_forwarding(&config);

        let bridge_harness = Harness::new();
        let bridge_manager = start_manager(
            &bridge_harness,
            &ManagerInputs {
                network: NetworkMode::Bridge,
                config: &config,
                services: &[],
                argv: &[
                    "codex".into(),
                    "--oss".into(),
                    "--local-provider=lmstudio".into(),
                ],
                settings: &settings,
            },
        )
        .await;
        let tcp_bridge = bridge_manager
            .plan()
            .init_services
            .iter()
            .find_map(|service| match service {
                InitServiceSpec::TcpBridge(service) => Some(service),
                _ => None,
            })
            .unwrap();
        assert!(tcp_bridge.proxy.is_none());
        assert_eq!(tcp_bridge.listen, "127.0.0.1:1234".parse().unwrap());
        assert_eq!(
            bridge_manager
                .plan()
                .env
                .get("LMSTUDIO_BASE_URL")
                .map(OsString::as_os_str),
            Some(OsStr::new("http://127.0.0.1:1234/v1"))
        );
        bridge_manager.shutdown().await.unwrap();

        let host_harness = Harness::new();
        let host_manager = start_manager(
            &host_harness,
            &ManagerInputs {
                network: NetworkMode::Host,
                config: &config,
                services: &[],
                argv: &["codex".into(), "--oss".into()],
                settings: &settings,
            },
        )
        .await;
        assert!(host_manager.plan().init_services.is_empty());
        assert!(host_manager.plan().mounts.is_empty());
        assert!(host_manager.tasks.is_empty());
        assert!(host_manager.authentication_directory.is_none());
        assert_eq!(
            host_manager
                .plan()
                .env
                .get("OLLAMA_HOST")
                .map(OsString::as_os_str),
            Some(OsStr::new("http://127.0.0.1:11434"))
        );
        host_manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loopback_declaration_uses_authenticated_relay_and_container_alias() {
        let harness = Harness::new();
        let config = disabled_forwarding();
        let settings = HostServiceSettings::from_forwarding(&config);
        let service = host_service(
            "localhost",
            9_001,
            Some("models.internal"),
            Some(19_001),
            false,
        );
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Bridge,
                config: &config,
                services: &[service],
                argv: &[],
                settings: &settings,
            },
        )
        .await;
        let bridge = manager
            .plan()
            .init_services
            .iter()
            .find_map(|service| match service {
                InitServiceSpec::TcpBridge(service) => Some(service),
                _ => None,
            })
            .unwrap();
        assert_eq!(bridge.listen, "127.0.0.1:19001".parse().unwrap());
        assert_eq!(
            manager
                .plan()
                .add_hosts
                .get("models.internal")
                .map(String::as_str),
            Some("127.0.0.1")
        );
        assert!(manager.plan().allow_private.contains(&bridge.remote));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remote_allowlist_service_needs_no_relay_token() {
        let harness = Harness::new();
        let config = disabled_forwarding();
        let settings = HostServiceSettings::from_forwarding(&config);
        let service = host_service(
            "models.example.test",
            443,
            Some("models.internal"),
            Some(8_443),
            false,
        );
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Allowlist,
                config: &config,
                services: &[service],
                argv: &[],
                settings: &settings,
            },
        )
        .await;
        assert!(matches!(
            manager.plan().init_services.as_slice(),
            [InitServiceSpec::Connect(_)]
        ));
        assert!(manager.plan().mounts.is_empty());
        assert!(manager.tasks.is_empty());
        assert!(manager.authentication_directory.is_none());
        assert_eq!(
            manager
                .plan()
                .add_hosts
                .get("models.internal")
                .map(String::as_str),
            Some("127.0.0.1")
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_oauth_url_drives_host_listener_env_and_codex_target() {
        let harness = Harness::new();
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let config = ForwardingConfig {
            browser: true,
            ..disabled_forwarding()
        };
        let native = CodexConfig {
            config: BTreeMap::from([(
                "mcp_oauth_callback_url".to_owned(),
                toml::Value::String(format!("http://127.0.0.1:{port}/oauth/base/")),
            )]),
            ..CodexConfig::default()
        }
        .mcp_oauth_callback(config.oauth_callback_port, &[])
        .unwrap();
        let mut settings = HostServiceSettings::from_forwarding(&config);
        settings.set_oauth_callback(&native);
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Bridge,
                config: &config,
                services: &[],
                argv: &[],
                settings: &settings,
            },
        )
        .await;
        let expected_url = format!("http://127.0.0.1:{port}/oauth/base/");
        assert_eq!(
            manager
                .plan()
                .env
                .get("CODEX_START_OAUTH_CALLBACK")
                .and_then(|value| value.to_str()),
            Some(expected_url.as_str())
        );
        let target = manager
            .plan()
            .init_services
            .iter()
            .find_map(|service| match service {
                InitServiceSpec::OauthTarget(service) => Some(service),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            target.callback,
            format!("127.0.0.1:{port}").parse().unwrap()
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn occupied_oauth_callback_degrades_to_browser_only() {
        let harness = Harness::new();
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let callback_port = occupied.local_addr().unwrap().port();
        let config = ForwardingConfig {
            browser: true,
            oauth_callback_port: callback_port,
            ..disabled_forwarding()
        };
        let settings = HostServiceSettings::from_forwarding(&config);
        let manager = start_manager(
            &harness,
            &ManagerInputs {
                network: NetworkMode::Bridge,
                config: &config,
                services: &[],
                argv: &[],
                settings: &settings,
            },
        )
        .await;
        assert!(manager.plan().env.contains_key("BROWSER"));
        assert!(
            !manager
                .plan()
                .init_services
                .iter()
                .any(|service| matches!(service, InitServiceSpec::OauthTarget(_)))
        );
        assert!(manager.plan().publish.is_empty());
        assert!(manager.plan().warnings.iter().any(|warning| {
            warning.contains("already in use") && warning.contains(&callback_port.to_string())
        }));
        assert_eq!(manager.tasks.len(), 1);
        manager.shutdown().await.unwrap();
        drop(occupied);
    }
}
