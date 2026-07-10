//! Data-driven, shell-free container process initialization.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    fs::OpenOptions,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{allowlist::Authority, auth::AuthToken};

const MAX_SPEC_BYTES: u64 = 1024 * 1024;
const MAX_SECRET_BYTES: u64 = 1024 * 1024;
const MAX_OWNERSHIP_ROOTS: usize = 64;
const MAX_OWNERSHIP_ENTRIES: usize = 100_000;
const MAX_OWNERSHIP_DEPTH: usize = 64;
const MAX_ACCOUNT_DATABASE_BYTES: u64 = 1024 * 1024;
const MAX_ACCOUNT_NAME_BYTES: usize = 32;
const SSH_FILES: &[&str] = &["config", "known_hosts", "known_hosts2", "allowed_signers"];
const PASSWD_PATH: &str = "/etc/passwd";
const GROUP_PATH: &str = "/etc/group";
const HTTP_PROXY_TOKEN_ENV: &str = "CODEX_START_HTTP_PROXY_TOKEN";
static ACCOUNT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One command represented as argv, never as shell source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Arguments excluding argv[0].
    #[serde(default)]
    pub args: Vec<String>,
    /// Non-secret additions to the command environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional command-specific working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// A Unix string encoded losslessly in the init JSON protocol.
///
/// UTF-8 values serialize as ordinary JSON strings. Other byte sequences use
/// `{ "unix_base64": "..." }`, keeping common specifications readable while
/// preserving arbitrary argv exactly.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OsValue(OsString);

impl OsValue {
    /// Returns the contained platform string.
    #[must_use]
    pub fn as_os_str(&self) -> &std::ffi::OsStr {
        &self.0
    }

    /// Consumes this wrapper and returns the exact platform string.
    #[must_use]
    pub fn into_os_string(self) -> OsString {
        self.0
    }
}

impl From<OsString> for OsValue {
    fn from(value: OsString) -> Self {
        Self(value)
    }
}

impl From<String> for OsValue {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl Serialize for OsValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(value) = self.0.to_str() {
            serializer.serialize_str(value)
        } else {
            BTreeMap::from([("unix_base64", BASE64.encode(self.0.as_os_str().as_bytes()))])
                .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for OsValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Encoded {
            unix_base64: String,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Representation {
            Utf8(String),
            UnixBase64(Encoded),
        }

        match Representation::deserialize(deserializer)? {
            Representation::Utf8(value) => Ok(Self(value.into())),
            Representation::UnixBase64(encoded) => BASE64
                .decode(encoded.unix_base64)
                .map(OsString::from_vec)
                .map(Self)
                .map_err(de::Error::custom),
        }
    }
}

/// Final workload command with byte-preserving Unix argv.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecSpec {
    /// Executable name or path.
    pub program: OsValue,
    /// Arguments excluding argv[0].
    #[serde(default)]
    pub args: Vec<OsValue>,
    /// Non-secret additions to the command environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional command-specific working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ExecSpec {
    /// Constructs an exact final command from program-plus-arguments argv.
    ///
    /// # Errors
    ///
    /// Returns an error when argv is empty, or the program/arguments contain a
    /// NUL byte that Unix cannot pass to `exec`.
    pub fn from_argv(argv: Vec<OsString>) -> Result<Self, InitError> {
        let mut argv = argv.into_iter();
        let program = argv.next().ok_or(InitError::InvalidCommand)?;
        let command = Self {
            program: program.into(),
            args: argv.map(OsValue::from).collect(),
            env: BTreeMap::new(),
            cwd: None,
        };
        validate_exec(&command)?;
        Ok(command)
    }

    /// Reconstructs exact program-plus-arguments argv.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        std::iter::once(self.program.0.clone())
            .chain(self.args.iter().map(|argument| argument.0.clone()))
            .collect()
    }
}

/// Safe preparation of SSH client configuration from mounted host files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshSetup {
    /// Read-only directory containing supported SSH configuration files.
    pub source: PathBuf,
    /// Destination `.ssh` directory used by the workload user.
    pub destination: PathBuf,
}

/// A workload-loopback service forwarded through the egress CONNECT proxy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectServiceSpec {
    /// Loopback-only listener visible to the workload.
    pub listen: std::net::SocketAddr,
    /// Egress proxy address, as `host:port`.
    pub proxy: String,
    /// Explicit destination authority, as `host:port`.
    pub target: String,
    /// Optional mounted bearer-token file for proxy authentication.
    #[serde(default)]
    pub auth_token_file: Option<PathBuf>,
    /// Maximum active clients for this service.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// CONNECT response timeout in seconds.
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

/// A workload-loopback HTTP proxy that authenticates to the managed egress
/// sidecar using a mounted per-run token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpProxyServiceSpec {
    /// Loopback-only listener advertised through proxy environment variables.
    pub listen: std::net::SocketAddr,
    /// Managed egress sidecar address, as `host:port`.
    pub proxy: String,
    /// Mounted bearer-token file. Its contents never enter argv or env.
    pub auth_token_file: PathBuf,
    /// Maximum active clients for this service.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Client request-head timeout in seconds.
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    /// Maximum accepted client request-head size.
    #[serde(default = "default_proxy_header_bytes")]
    pub max_header_bytes: usize,
}

/// A workload-loopback service forwarded directly in bridge/host modes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TcpForwardServiceSpec {
    /// Loopback-only listener visible to the workload.
    pub listen: std::net::SocketAddr,
    /// One explicit direct destination authority.
    pub target: String,
    /// Maximum active clients for this service.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

/// A workload-loopback TCP service backed by an authenticated host relay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TcpBridgeServiceSpec {
    /// Loopback-only listener visible to the workload.
    pub listen: std::net::SocketAddr,
    /// Authenticated host relay authority as `host:port`.
    pub remote: String,
    /// Mounted relay authentication token.
    pub token_file: PathBuf,
    /// Optional egress proxy used to reach `remote`.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Optional mounted bearer token used by the egress proxy.
    #[serde(default)]
    pub proxy_auth_token_file: Option<PathBuf>,
    /// Maximum active clients for this service.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Authentication/CONNECT timeout in seconds.
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

/// A Unix socket backed by an authenticated TCP relay, optionally reached
/// through the egress HTTP CONNECT proxy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnixBridgeServiceSpec {
    /// Owned Unix socket created for the workload (for example `SSH_AUTH_SOCK`).
    pub listen: PathBuf,
    /// Authenticated host relay authority as `host:port`.
    pub remote: String,
    /// Mounted relay authentication token.
    pub token_file: PathBuf,
    /// Optional egress proxy address used to reach `remote`.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Optional mounted bearer token used by the egress proxy.
    #[serde(default)]
    pub proxy_auth_token_file: Option<PathBuf>,
    /// Maximum active clients for this service.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Authentication/CONNECT timeout in seconds.
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

/// Authenticated reverse OAuth relay targeting a workload loopback callback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthTargetServiceSpec {
    /// Authenticated relay listener, commonly published only on host loopback.
    pub listen: std::net::SocketAddr,
    /// Workload callback server; must be loopback with a non-zero port.
    pub callback: std::net::SocketAddr,
    /// Mounted relay authentication token.
    pub token_file: PathBuf,
    /// Maximum active browser callback connections.
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    /// Target connection timeout in seconds.
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// Relay authentication timeout in seconds.
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    /// Bidirectional inactivity timeout in seconds.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

/// Long-lived helper service made ready before preparation and workload execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InitServiceSpec {
    /// Loopback HTTP proxy that injects managed-sidecar authentication.
    HttpProxy(HttpProxyServiceSpec),
    /// Loopback TCP service reached through HTTP CONNECT.
    Connect(ConnectServiceSpec),
    /// Loopback TCP service reached directly in bridge/host modes.
    TcpForward(TcpForwardServiceSpec),
    /// Loopback TCP service reached through an authenticated host relay.
    TcpBridge(TcpBridgeServiceSpec),
    /// Unix socket reached through an authenticated host TCP relay.
    UnixBridge(UnixBridgeServiceSpec),
    /// Authenticated host callback forwarded to workload loopback.
    OauthTarget(OAuthTargetServiceSpec),
}

const fn default_service_connections() -> usize {
    64
}

const fn default_connect_timeout_seconds() -> u64 {
    10
}

const fn default_handshake_timeout_seconds() -> u64 {
    10
}

const fn default_idle_timeout_seconds() -> u64 {
    300
}

const fn default_proxy_header_bytes() -> usize {
    65_536
}

/// Versioned JSON initialization specification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitSpec {
    /// Schema version; v0 supports version 1.
    pub version: u32,
    /// Target workload UID. Must be paired with `gid`.
    pub uid: Option<u32>,
    /// Target workload GID. Must be paired with `uid`.
    pub gid: Option<u32>,
    /// Existing container account remapped to `uid`/`gid` before setup.
    #[serde(default)]
    pub account: Option<String>,
    /// Final command working directory.
    pub cwd: Option<PathBuf>,
    /// Clear the inherited container environment before applying explicit values.
    #[serde(default)]
    pub clear_environment: bool,
    /// Non-secret environment values.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// JSON map of environment names to mounted secret file paths.
    pub secret_map: Option<PathBuf>,
    /// Directory under which every mapped secret must reside.
    #[serde(default = "default_secret_root")]
    pub secret_root: PathBuf,
    /// Permit group/world-accessible secret files. Intended only for runtimes
    /// whose secret mounts cannot preserve mode bits.
    #[serde(default)]
    pub allow_insecure_secret_permissions: bool,
    /// Container-owned writable roots recursively assigned to `uid`/`gid`
    /// without crossing symlinks or nested filesystems.
    #[serde(default)]
    pub ownership_paths: Vec<PathBuf>,
    /// Optional safe SSH configuration materialization.
    pub ssh: Option<SshSetup>,
    /// Sequential argv-only setup commands.
    #[serde(default)]
    pub prepare: Vec<CommandSpec>,
    /// Loopback services made ready before preparation and the final workload.
    #[serde(default)]
    pub services: Vec<InitServiceSpec>,
    /// Process that replaces the init helper.
    pub command: ExecSpec,
}

/// Direct-command options used to construct an [`InitSpec`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectInitOptions {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub cwd: Option<PathBuf>,
    pub secret_map: Option<PathBuf>,
    pub ssh: Option<SshSetup>,
    pub prepare: Vec<CommandSpec>,
    pub allow_insecure_secret_permissions: bool,
    pub clear_environment: bool,
    pub ownership_paths: Vec<PathBuf>,
}

fn default_secret_root() -> PathBuf {
    PathBuf::from("/run/codex-start/secrets")
}

/// Secret environment loaded into zeroizing buffers.
pub struct SecretEnvironment(Vec<(String, Zeroizing<Vec<u8>>)>);

impl SecretEnvironment {
    /// Returns the number of loaded secret variables without exposing values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether no secrets were loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn apply(&self, command: &mut Command) {
        for (name, value) in &self.0 {
            command.env(name, OsString::from_vec(value.to_vec()));
        }
    }
}

impl std::fmt::Debug for SecretEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretEnvironment")
            .field(
                "variables",
                &self.0.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("values", &"[REDACTED]")
            .finish()
    }
}

/// Loads and strictly validates an initialization specification.
///
/// # Errors
///
/// Returns an error when the file is unsafe/unreadable, JSON is invalid, or
/// the specification violates an init invariant.
pub fn load_init_spec(path: &Path) -> Result<InitSpec, InitError> {
    // The specification contains paths and non-secret settings only, so it may
    // be delivered through a conventional read-only 0444/0644 config mount.
    let bytes = read_bounded_regular_file(path, MAX_SPEC_BYTES, true)?;
    let spec: InitSpec = serde_json::from_slice(&bytes).map_err(|source| InitError::ParseSpec {
        path: path.to_owned(),
        source,
    })?;
    validate_spec(&spec)?;
    Ok(spec)
}

/// Validates all static invariants in a specification.
///
/// # Errors
///
/// Returns an error for unsupported schemas, incomplete identities, unsafe
/// roots, invalid environment names, or invalid commands.
pub fn validate_spec(spec: &InitSpec) -> Result<(), InitError> {
    if spec.version != 1 {
        return Err(InitError::UnsupportedVersion(spec.version));
    }
    if spec.uid.is_some() != spec.gid.is_some() {
        return Err(InitError::UidGidPairRequired);
    }
    if let Some(account) = &spec.account {
        validate_account_name(account)?;
        if spec.uid.is_none() {
            return Err(InitError::AccountRequiresIdentity);
        }
    }
    if !spec.ownership_paths.is_empty() && spec.uid.is_none() {
        return Err(InitError::OwnershipRequiresIdentity);
    }
    validate_ownership_paths(&spec.ownership_paths)?;
    validate_exec(&spec.command)?;
    for command in &spec.prepare {
        validate_command(command)?;
    }
    for name in spec.env.keys() {
        validate_environment_name(name)?;
    }
    let mut service_listeners = HashSet::new();
    for service in &spec.services {
        match service {
            InitServiceSpec::HttpProxy(service) => {
                validate_http_proxy_service(service, &spec.secret_root)?;
                if !service_listeners.insert(format!("tcp:{}", service.listen)) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen
                    )));
                }
            }
            InitServiceSpec::Connect(service) => {
                validate_connect_service(service, &spec.secret_root)?;
                if !service_listeners.insert(format!("tcp:{}", service.listen)) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen
                    )));
                }
            }
            InitServiceSpec::TcpForward(service) => {
                validate_tcp_forward_service(service)?;
                if !service_listeners.insert(format!("tcp:{}", service.listen)) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen
                    )));
                }
            }
            InitServiceSpec::TcpBridge(service) => {
                validate_tcp_bridge_service(service, &spec.secret_root)?;
                if !service_listeners.insert(format!("tcp:{}", service.listen)) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen
                    )));
                }
            }
            InitServiceSpec::UnixBridge(service) => {
                validate_unix_service(service, &spec.secret_root)?;
                if !service_listeners.insert(format!("unix:{}", service.listen.display())) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen.display()
                    )));
                }
            }
            InitServiceSpec::OauthTarget(service) => {
                validate_oauth_service(service, &spec.secret_root)?;
                if !service_listeners.insert(format!("tcp:{}", service.listen)) {
                    return Err(InitError::InvalidService(format!(
                        "duplicate listener {}",
                        service.listen
                    )));
                }
            }
        }
    }
    if spec.secret_root.as_os_str().is_empty() || !spec.secret_root.is_absolute() {
        return Err(InitError::InvalidSecretRoot(spec.secret_root.clone()));
    }
    Ok(())
}

fn validate_http_proxy_service(
    service: &HttpProxyServiceSpec,
    secret_root: &Path,
) -> Result<(), InitError> {
    if !service.listen.ip().is_loopback() || service.listen.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "HTTP proxy listener {} must use loopback and a non-zero port",
            service.listen
        )));
    }
    validate_host_port_text(&service.proxy, "proxy")?;
    validate_service_token(&service.auth_token_file, secret_root)?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )?;
    if service.max_header_bytes < 1_024 {
        return Err(InitError::InvalidService(
            "HTTP proxy max_header_bytes must be at least 1024".to_owned(),
        ));
    }
    Ok(())
}

fn validate_connect_service(
    service: &ConnectServiceSpec,
    secret_root: &Path,
) -> Result<(), InitError> {
    if !service.listen.ip().is_loopback() || service.listen.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "listener {} must use a loopback address and non-zero port",
            service.listen
        )));
    }
    validate_host_port_text(&service.proxy, "proxy")?;
    Authority::parse(&service.target, None).map_err(|error| {
        InitError::InvalidService(format!("invalid target {}: {error}", service.target))
    })?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )?;
    if let Some(path) = &service.auth_token_file {
        validate_service_token(path, secret_root)?;
    }
    Ok(())
}

fn validate_tcp_forward_service(service: &TcpForwardServiceSpec) -> Result<(), InitError> {
    if !service.listen.ip().is_loopback() || service.listen.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "listener {} must use a loopback address and non-zero port",
            service.listen
        )));
    }
    validate_host_port_text(&service.target, "direct target")?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        1,
        service.idle_timeout_seconds,
    )
}

fn validate_tcp_bridge_service(
    service: &TcpBridgeServiceSpec,
    secret_root: &Path,
) -> Result<(), InitError> {
    if !service.listen.ip().is_loopback() || service.listen.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "listener {} must use a loopback address and non-zero port",
            service.listen
        )));
    }
    validate_authenticated_bridge(
        &AuthenticatedBridgeValidation {
            remote: &service.remote,
            token_file: &service.token_file,
            proxy: service.proxy.as_deref(),
            proxy_auth_token_file: service.proxy_auth_token_file.as_deref(),
            max_connections: service.max_connections,
            connect_timeout_seconds: service.connect_timeout_seconds,
            handshake_timeout_seconds: service.handshake_timeout_seconds,
            idle_timeout_seconds: service.idle_timeout_seconds,
        },
        secret_root,
    )
}

fn validate_unix_service(
    service: &UnixBridgeServiceSpec,
    secret_root: &Path,
) -> Result<(), InitError> {
    use std::path::Component;

    let safe_prefixes = ["/tmp", "/var/tmp", "/run/codex-start", "/home"];
    if !service.listen.is_absolute()
        || service
            .listen
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || !safe_prefixes
            .iter()
            .any(|prefix| service.listen.starts_with(prefix))
        || service.listen.file_name().is_none()
    {
        return Err(InitError::InvalidService(format!(
            "unsafe Unix listener {}",
            service.listen.display()
        )));
    }
    validate_authenticated_bridge(
        &AuthenticatedBridgeValidation {
            remote: &service.remote,
            token_file: &service.token_file,
            proxy: service.proxy.as_deref(),
            proxy_auth_token_file: service.proxy_auth_token_file.as_deref(),
            max_connections: service.max_connections,
            connect_timeout_seconds: service.connect_timeout_seconds,
            handshake_timeout_seconds: service.handshake_timeout_seconds,
            idle_timeout_seconds: service.idle_timeout_seconds,
        },
        secret_root,
    )
}

struct AuthenticatedBridgeValidation<'a> {
    remote: &'a str,
    token_file: &'a Path,
    proxy: Option<&'a str>,
    proxy_auth_token_file: Option<&'a Path>,
    max_connections: usize,
    connect_timeout_seconds: u64,
    handshake_timeout_seconds: u64,
    idle_timeout_seconds: u64,
}

fn validate_authenticated_bridge(
    bridge: &AuthenticatedBridgeValidation<'_>,
    secret_root: &Path,
) -> Result<(), InitError> {
    validate_host_port_text(bridge.remote, "authenticated relay")?;
    Authority::parse(bridge.remote, None).map_err(|error| {
        InitError::InvalidService(format!("invalid relay {}: {error}", bridge.remote))
    })?;
    validate_service_token(bridge.token_file, secret_root)?;
    if let Some(proxy) = bridge.proxy {
        validate_host_port_text(proxy, "proxy")?;
    }
    if let Some(path) = bridge.proxy_auth_token_file {
        if bridge.proxy.is_none() {
            return Err(InitError::InvalidService(
                "proxy_auth_token_file requires proxy".to_owned(),
            ));
        }
        validate_service_token(path, secret_root)?;
    }
    validate_service_limits(
        bridge.max_connections,
        bridge.connect_timeout_seconds,
        bridge.handshake_timeout_seconds,
        bridge.idle_timeout_seconds,
    )
}

fn validate_oauth_service(
    service: &OAuthTargetServiceSpec,
    secret_root: &Path,
) -> Result<(), InitError> {
    if service.listen.port() == 0 {
        return Err(InitError::InvalidService(
            "OAuth relay listener requires a non-zero port".to_owned(),
        ));
    }
    if !service.callback.ip().is_loopback() || service.callback.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "OAuth callback {} must use loopback and a non-zero port",
            service.callback
        )));
    }
    validate_service_token(&service.token_file, secret_root)?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )
}

fn validate_host_port_text(value: &str, label: &str) -> Result<(), InitError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(InitError::InvalidService(format!(
            "{label} must be a non-empty host:port"
        )));
    }
    Authority::parse(value, None)
        .map_err(|error| InitError::InvalidService(format!("invalid {label} {value}: {error}")))?;
    Ok(())
}

fn validate_service_token(path: &Path, secret_root: &Path) -> Result<(), InitError> {
    const INTERNAL_SECRET_ROOT: &str = "/run/codex-start/secrets";
    let under_secret_root = path.starts_with(secret_root) && path != secret_root;
    let internal_root = Path::new(INTERNAL_SECRET_ROOT);
    let under_internal_root = path.starts_with(internal_root) && path != internal_root;
    if !path.is_absolute() || (!under_secret_root && !under_internal_root) {
        return Err(InitError::InvalidService(format!(
            "service token file must be under {} or {INTERNAL_SECRET_ROOT}",
            secret_root.display(),
        )));
    }
    Ok(())
}

fn validate_service_limits(
    max_connections: usize,
    connect_timeout: u64,
    handshake_timeout: u64,
    idle_timeout: u64,
) -> Result<(), InitError> {
    if max_connections == 0 || connect_timeout == 0 || handshake_timeout == 0 || idle_timeout == 0 {
        return Err(InitError::InvalidService(
            "service limits and timeouts must be non-zero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_command(command: &CommandSpec) -> Result<(), InitError> {
    if command.program.is_empty() || command.program.as_bytes().contains(&0) {
        return Err(InitError::InvalidCommand);
    }
    for name in command.env.keys() {
        validate_environment_name(name)?;
    }
    Ok(())
}

fn validate_exec(command: &ExecSpec) -> Result<(), InitError> {
    if command.program.as_os_str().as_bytes().is_empty()
        || command.program.as_os_str().as_bytes().contains(&0)
        || command
            .args
            .iter()
            .any(|argument| argument.as_os_str().as_bytes().contains(&0))
    {
        return Err(InitError::InvalidCommand);
    }
    for name in command.env.keys() {
        validate_environment_name(name)?;
    }
    Ok(())
}

fn validate_account_name(account: &str) -> Result<(), InitError> {
    let bytes = account.as_bytes();
    let first_is_valid = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    let rest_is_valid = bytes.get(1..).is_some_and(|rest| {
        rest.iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    });
    if bytes.len() > MAX_ACCOUNT_NAME_BYTES || !first_is_valid || !rest_is_valid {
        return Err(InitError::InvalidAccountName);
    }
    Ok(())
}

fn remap_account(account: &str, uid: u32, gid: u32) -> Result<(), InitError> {
    remap_account_files(
        Path::new(PASSWD_PATH),
        Path::new(GROUP_PATH),
        account,
        uid,
        gid,
    )
}

fn remap_account_files(
    passwd_path: &Path,
    group_path: &Path,
    account: &str,
    uid: u32,
    gid: u32,
) -> Result<(), InitError> {
    validate_account_name(account)?;
    let passwd = read_account_database(passwd_path)?;
    let group = read_account_database(group_path)?;
    let rewritten_passwd = rewrite_passwd(&passwd.bytes, passwd_path, account, uid, gid)?;
    let rewritten_group = rewrite_group(&group.bytes, group_path, account, gid)?;

    // Both replacements are fully prepared before either live database is
    // touched. Each changed file is then committed with a same-directory
    // atomic rename, so readers never observe a partially written record.
    if rewritten_group != group.bytes {
        atomic_replace_account_database(group_path, &rewritten_group, &group.permissions)?;
    }
    if rewritten_passwd != passwd.bytes {
        atomic_replace_account_database(passwd_path, &rewritten_passwd, &passwd.permissions)?;
    }
    Ok(())
}

struct AccountDatabase {
    bytes: Vec<u8>,
    permissions: std::fs::Permissions,
}

fn read_account_database(path: &Path) -> Result<AccountDatabase, InitError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| InitError::AccountIo {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: "not a regular file".to_owned(),
        });
    }
    if metadata.len() > MAX_ACCOUNT_DATABASE_BYTES {
        return Err(InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: format!(
                "file is {} bytes; maximum is {MAX_ACCOUNT_DATABASE_BYTES}",
                metadata.len()
            ),
        });
    }
    let bytes = std::fs::read(path).map_err(|source| InitError::AccountIo {
        path: path.to_owned(),
        source,
    })?;
    Ok(AccountDatabase {
        bytes,
        permissions: metadata.permissions(),
    })
}

fn rewrite_passwd(
    bytes: &[u8],
    path: &Path,
    account: &str,
    uid: u32,
    gid: u32,
) -> Result<Vec<u8>, InitError> {
    let account_bytes = account.as_bytes();
    let mut matching_records = 0_usize;
    for (line_number, segment) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let Some(fields) = parse_database_record(segment, path, line_number + 1, 7)? else {
            continue;
        };
        let record_uid = parse_database_id(fields[2], path, line_number + 1)?;
        if fields[0] == account_bytes {
            matching_records += 1;
        } else if record_uid == uid {
            return Err(InitError::UidConflict {
                uid,
                account: String::from_utf8_lossy(fields[0]).into_owned(),
            });
        }
    }
    match matching_records {
        0 => return Err(InitError::AccountNotFound(account.to_owned())),
        1 => {}
        _ => return Err(InitError::DuplicateAccount(account.to_owned())),
    }

    rewrite_database(bytes, path, 7, |fields, output| {
        if fields[0] == account_bytes {
            append_fields_with_ids(output, fields, uid, gid);
            true
        } else {
            false
        }
    })
}

fn rewrite_group(bytes: &[u8], path: &Path, account: &str, gid: u32) -> Result<Vec<u8>, InitError> {
    let account_bytes = account.as_bytes();
    let mut matching_records = 0_usize;
    let mut gid_conflict = false;
    for (line_number, segment) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let Some(fields) = parse_database_record(segment, path, line_number + 1, 4)? else {
            continue;
        };
        let record_gid = parse_database_id(fields[2], path, line_number + 1)?;
        if fields[0] == account_bytes {
            matching_records += 1;
        } else if record_gid == gid {
            gid_conflict = true;
        }
    }
    if matching_records > 1 {
        return Err(InitError::DuplicateGroup(account.to_owned()));
    }
    if matching_records == 0 || gid_conflict {
        return Ok(bytes.to_vec());
    }

    rewrite_database(bytes, path, 4, |fields, output| {
        if fields[0] == account_bytes {
            append_field(output, fields[0]);
            append_field(output, fields[1]);
            output.extend_from_slice(gid.to_string().as_bytes());
            output.push(b':');
            output.extend_from_slice(fields[3]);
            true
        } else {
            false
        }
    })
}

fn rewrite_database<F>(
    bytes: &[u8],
    path: &Path,
    expected_fields: usize,
    mut replace: F,
) -> Result<Vec<u8>, InitError>
where
    F: FnMut(&[&[u8]], &mut Vec<u8>) -> bool,
{
    let mut output = Vec::with_capacity(bytes.len());
    for (line_number, segment) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let Some(fields) = parse_database_record(segment, path, line_number + 1, expected_fields)?
        else {
            output.extend_from_slice(segment);
            continue;
        };
        if !replace(&fields, &mut output) {
            output.extend_from_slice(segment);
            continue;
        }
        if segment.ends_with(b"\n") {
            output.push(b'\n');
        }
    }
    Ok(output)
}

fn parse_database_record<'a>(
    segment: &'a [u8],
    path: &Path,
    line_number: usize,
    expected_fields: usize,
) -> Result<Option<Vec<&'a [u8]>>, InitError> {
    let line = segment.strip_suffix(b"\n").unwrap_or(segment);
    if line.is_empty() || line.starts_with(b"#") {
        return Ok(None);
    }
    let fields = line.split(|byte| *byte == b':').collect::<Vec<_>>();
    if fields.len() != expected_fields || fields[0].is_empty() {
        return Err(InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: format!("malformed record at line {line_number}"),
        });
    }
    Ok(Some(fields))
}

fn parse_database_id(field: &[u8], path: &Path, line_number: usize) -> Result<u32, InitError> {
    std::str::from_utf8(field)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: format!("invalid numeric identifier at line {line_number}"),
        })
}

fn append_field(output: &mut Vec<u8>, field: &[u8]) {
    output.extend_from_slice(field);
    output.push(b':');
}

fn append_fields_with_ids(output: &mut Vec<u8>, fields: &[&[u8]], uid: u32, gid: u32) {
    append_field(output, fields[0]);
    append_field(output, fields[1]);
    append_field(output, uid.to_string().as_bytes());
    append_field(output, gid.to_string().as_bytes());
    for (index, field) in fields[4..].iter().enumerate() {
        output.extend_from_slice(field);
        if index + 1 < fields.len() - 4 {
            output.push(b':');
        }
    }
}

fn atomic_replace_account_database(
    path: &Path,
    bytes: &[u8],
    permissions: &std::fs::Permissions,
) -> Result<(), InitError> {
    let (temporary_path, mut temporary) = create_account_temporary(path)?;
    let result = (|| {
        temporary
            .write_all(bytes)
            .and_then(|()| temporary.set_permissions(permissions.clone()))
            .and_then(|()| temporary.sync_all())
            .map_err(|source| InitError::AccountIo {
                path: temporary_path.clone(),
                source,
            })?;
        drop(temporary);
        std::fs::rename(&temporary_path, path).map_err(|source| InitError::AccountIo {
            path: path.to_owned(),
            source,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn create_account_temporary(path: &Path) -> Result<(PathBuf, std::fs::File), InitError> {
    let parent = path
        .parent()
        .ok_or_else(|| InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: "database has no parent directory".to_owned(),
        })?;
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| InitError::InvalidAccountDatabase {
            path: path.to_owned(),
            reason: "database has an invalid file name".to_owned(),
        })?;
    for _ in 0..128 {
        let sequence = ACCOUNT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.codex-start-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(InitError::AccountIo {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(InitError::InvalidAccountDatabase {
        path: path.to_owned(),
        reason: "could not allocate a private temporary file".to_owned(),
    })
}

fn validate_ownership_paths(paths: &[PathBuf]) -> Result<(), InitError> {
    use std::path::Component;

    if paths.len() > MAX_OWNERSHIP_ROOTS {
        return Err(InitError::InvalidOwnershipPath(format!(
            "at most {MAX_OWNERSHIP_ROOTS} ownership roots are permitted"
        )));
    }
    let denied = [
        "/bin", "/dev", "/etc", "/lib", "/lib64", "/proc", "/run", "/sbin", "/sys", "/usr",
    ];
    for path in paths {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            || path == Path::new("/")
            || denied
                .iter()
                .any(|denied| path.starts_with(Path::new(denied)))
        {
            return Err(InitError::InvalidOwnershipPath(path.display().to_string()));
        }
    }
    for (index, path) in paths.iter().enumerate() {
        if paths[..index].contains(path) {
            return Err(InitError::InvalidOwnershipPath(format!(
                "ownership root is duplicated at {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Result of safe recursive ownership preparation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OwnershipReport {
    /// Files and directories whose ownership was updated.
    pub changed: usize,
    /// Symbolic links deliberately left untouched.
    pub skipped_symlinks: usize,
    /// Nested filesystem entries deliberately not traversed or changed.
    pub skipped_filesystems: usize,
}

/// Recursively assigns container-owned roots without following symlinks.
///
/// Missing roots and their parent directories are created before traversal.
/// Traversal is bounded, remains under each lexical root, and does not cross
/// filesystem device boundaries (which protects nested bind mounts).
///
/// # Errors
///
/// Returns an error for unsafe/duplicate roots, non-directory roots, bounds
/// exhaustion, metadata/traversal failures, or ownership-change failures.
pub fn prepare_ownership(
    paths: &[PathBuf],
    uid: u32,
    gid: u32,
) -> Result<OwnershipReport, InitError> {
    validate_ownership_paths(paths)?;
    for root in paths {
        create_ownership_root(root)?;
    }
    let mut report = OwnershipReport::default();
    for root in paths {
        let metadata =
            std::fs::symlink_metadata(root).map_err(|source| InitError::OwnershipIo {
                path: root.clone(),
                source,
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(InitError::InvalidOwnershipPath(format!(
                "{} is not a real directory",
                root.display()
            )));
        }
        let root_device = metadata.dev();
        let mut pending = vec![(root.clone(), 0_usize)];
        while let Some((path, depth)) = pending.pop() {
            if report.changed + report.skipped_symlinks + report.skipped_filesystems
                >= MAX_OWNERSHIP_ENTRIES
            {
                return Err(InitError::OwnershipLimit(MAX_OWNERSHIP_ENTRIES));
            }
            if depth > MAX_OWNERSHIP_DEPTH {
                return Err(InitError::OwnershipDepth {
                    path,
                    maximum: MAX_OWNERSHIP_DEPTH,
                });
            }
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|source| InitError::OwnershipIo {
                    path: path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                report.skipped_symlinks += 1;
                continue;
            }
            if metadata.dev() != root_device {
                report.skipped_filesystems += 1;
                continue;
            }
            if metadata.is_dir() {
                for entry in std::fs::read_dir(&path).map_err(|source| InitError::OwnershipIo {
                    path: path.clone(),
                    source,
                })? {
                    let entry = entry.map_err(|source| InitError::OwnershipIo {
                        path: path.clone(),
                        source,
                    })?;
                    let child = entry.path();
                    if !child.starts_with(root) {
                        return Err(InitError::InvalidOwnershipPath(child.display().to_string()));
                    }
                    pending.push((child, depth + 1));
                }
            }
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid)).map_err(|source| {
                InitError::OwnershipIo {
                    path: path.clone(),
                    source,
                }
            })?;
            report.changed += 1;
        }
    }
    Ok(report)
}

fn create_ownership_root(root: &Path) -> Result<(), InitError> {
    use std::path::Component;

    let mut current = PathBuf::from("/");
    for component in root.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(InitError::InvalidOwnershipPath(format!(
                    "{} is not a real directory",
                    current.display()
                )));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|source| InitError::OwnershipIo {
                    path: current.clone(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(InitError::OwnershipIo {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

/// Loads a JSON object mapping environment variable names to secret files.
///
/// # Errors
///
/// Returns an error for unsafe files or permissions, invalid JSON/names,
/// mappings outside `secret_root`, over-sized values, or NUL bytes.
pub fn load_secret_map(
    map_path: &Path,
    secret_root: &Path,
    allow_insecure_permissions: bool,
) -> Result<SecretEnvironment, InitError> {
    let bytes = read_bounded_regular_file(map_path, MAX_SPEC_BYTES, allow_insecure_permissions)?;
    let mappings: BTreeMap<String, PathBuf> =
        serde_json::from_slice(&bytes).map_err(|source| InitError::ParseSecretMap {
            path: map_path.to_owned(),
            source,
        })?;
    let canonical_root = secret_root
        .canonicalize()
        .map_err(|source| InitError::Canonicalize {
            path: secret_root.to_owned(),
            source,
        })?;
    let mut secrets = Vec::with_capacity(mappings.len());
    for (name, path) in mappings {
        validate_environment_name(&name)?;
        if !path.is_absolute() {
            return Err(InitError::SecretOutsideRoot { name, path });
        }
        let canonical_path = path
            .canonicalize()
            .map_err(|source| InitError::Canonicalize {
                path: path.clone(),
                source,
            })?;
        if !canonical_path.starts_with(&canonical_root) || canonical_path == canonical_root {
            return Err(InitError::SecretOutsideRoot {
                name,
                path: canonical_path,
            });
        }
        let mut value = read_bounded_regular_file(
            &canonical_path,
            MAX_SECRET_BYTES,
            allow_insecure_permissions,
        )?;
        if value.ends_with(b"\r\n") {
            value.truncate(value.len() - 2);
        } else if value.ends_with(b"\n") {
            value.truncate(value.len() - 1);
        }
        if value.contains(&0) {
            return Err(InitError::SecretContainsNul(name));
        }
        secrets.push((name, Zeroizing::new(value)));
    }
    Ok(SecretEnvironment(secrets))
}

fn read_bounded_regular_file(
    path: &Path,
    maximum: u64,
    allow_insecure_permissions: bool,
) -> Result<Vec<u8>, InitError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| InitError::Read {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(InitError::NotRegularFile(path.to_owned()));
    }
    if metadata.len() > maximum {
        return Err(InitError::FileTooLarge {
            path: path.to_owned(),
            length: metadata.len(),
            maximum,
        });
    }
    if !allow_insecure_permissions && metadata.mode() & 0o077 != 0 {
        return Err(InitError::InsecurePermissions {
            path: path.to_owned(),
            mode: metadata.mode() & 0o777,
        });
    }
    std::fs::read(path).map_err(|source| InitError::Read {
        path: path.to_owned(),
        source,
    })
}

fn validate_environment_name(name: &str) -> Result<(), InitError> {
    let mut bytes = name.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !valid_start || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err(InitError::InvalidEnvironmentName(name.to_owned()));
    }
    Ok(())
}

/// Copies supported SSH client configuration without following symlinks.
///
/// # Errors
///
/// Returns an error when a source/destination is unsafe or file preparation,
/// permission changes, or ownership changes fail.
pub fn prepare_ssh_config(setup: &SshSetup, uid_gid: Option<(u32, u32)>) -> Result<(), InitError> {
    let source_metadata =
        std::fs::symlink_metadata(&setup.source).map_err(|source| InitError::PrepareSsh {
            path: setup.source.clone(),
            source,
        })?;
    if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
        return Err(InitError::UnsafeSshPath(setup.source.clone()));
    }
    ensure_safe_destination_directory(&setup.destination)?;
    std::fs::set_permissions(&setup.destination, std::fs::Permissions::from_mode(0o700)).map_err(
        |source| InitError::PrepareSsh {
            path: setup.destination.clone(),
            source,
        },
    )?;
    if let Some((uid, gid)) = uid_gid {
        std::os::unix::fs::chown(&setup.destination, Some(uid), Some(gid)).map_err(|source| {
            InitError::PrepareSsh {
                path: setup.destination.clone(),
                source,
            }
        })?;
    }

    for filename in SSH_FILES {
        let source = setup.source.join(filename);
        let metadata = match std::fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source_error) => {
                return Err(InitError::PrepareSsh {
                    path: source,
                    source: source_error,
                });
            }
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(InitError::UnsafeSshPath(source));
        }
        if metadata.len() > MAX_SPEC_BYTES {
            return Err(InitError::FileTooLarge {
                path: source,
                length: metadata.len(),
                maximum: MAX_SPEC_BYTES,
            });
        }
        let destination = setup.destination.join(filename);
        if let Ok(existing) = std::fs::symlink_metadata(&destination)
            && (!existing.file_type().is_file() || existing.file_type().is_symlink())
        {
            return Err(InitError::UnsafeSshPath(destination));
        }
        std::fs::copy(&source, &destination).map_err(|source| InitError::PrepareSsh {
            path: destination.clone(),
            source,
        })?;
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| InitError::PrepareSsh {
                path: destination.clone(),
                source,
            },
        )?;
        if let Some((uid, gid)) = uid_gid {
            std::os::unix::fs::chown(&destination, Some(uid), Some(gid)).map_err(|source| {
                InitError::PrepareSsh {
                    path: destination,
                    source,
                }
            })?;
        }
    }
    Ok(())
}

fn ensure_safe_destination_directory(path: &Path) -> Result<(), InitError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(InitError::UnsafeSshPath(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| InitError::UnsafeSshPath(path.to_owned()))?;
            let parent_metadata =
                std::fs::symlink_metadata(parent).map_err(|source| InitError::PrepareSsh {
                    path: parent.to_owned(),
                    source,
                })?;
            if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
                return Err(InitError::UnsafeSshPath(parent.to_owned()));
            }
            std::fs::create_dir(path).map_err(|source| InitError::PrepareSsh {
                path: path.to_owned(),
                source,
            })
        }
        Err(source) => Err(InitError::PrepareSsh {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Runs setup commands with the exact final environment and credentials.
///
/// # Errors
///
/// Returns an error when a command cannot start or exits unsuccessfully.
pub fn run_prepare_commands(spec: &InitSpec, secrets: &SecretEnvironment) -> Result<(), InitError> {
    for command in &spec.prepare {
        let mut process = build_command(spec, command, secrets);
        let status = process.status().map_err(|source| InitError::SpawnPrepare {
            program: command.program.clone(),
            source,
        })?;
        if !status.success() {
            return Err(InitError::PrepareFailed {
                program: command.program.clone(),
                status,
            });
        }
    }
    Ok(())
}

fn build_command(spec: &InitSpec, command: &CommandSpec, secrets: &SecretEnvironment) -> Command {
    let mut process = Command::new(&command.program);
    process.args(&command.args);
    if spec.clear_environment {
        process.env_clear();
    }
    process.envs(&spec.env);
    process.envs(&command.env);
    secrets.apply(&mut process);
    if let Some(cwd) = command.cwd.as_ref().or(spec.cwd.as_ref()) {
        process.current_dir(cwd);
    }
    apply_identity(&mut process, spec.uid.zip(spec.gid));
    process
}

fn build_exec_command(spec: &InitSpec, command: &ExecSpec, secrets: &SecretEnvironment) -> Command {
    let mut process = Command::new(command.program.as_os_str());
    process.args(command.args.iter().map(OsValue::as_os_str));
    if spec.clear_environment {
        process.env_clear();
    }
    process.envs(&spec.env);
    process.envs(&command.env);
    secrets.apply(&mut process);
    if let Some(cwd) = command.cwd.as_ref().or(spec.cwd.as_ref()) {
        process.current_dir(cwd);
    }
    apply_identity(&mut process, spec.uid.zip(spec.gid));
    process
}

fn apply_identity(process: &mut Command, identity: Option<(u32, u32)>) {
    if let Some((uid, gid)) = identity {
        // `CommandExt::uid` also performs `setgroups(0, NULL)` before dropping
        // privileges, preventing inherited supplementary groups from leaking
        // into preparation commands, helpers, or the final workload.
        process.gid(gid).uid(uid);
    }
}

/// Performs SSH setup and preparation, then replaces the current process.
///
/// On success this function does not return.
///
/// # Errors
///
/// Returns an error if validation, secret loading, SSH setup, preparation, or
/// the final `exec` operation fails.
pub fn run_and_exec(spec: &InitSpec) -> Result<(), InitError> {
    validate_spec(spec)?;
    let secrets = match &spec.secret_map {
        Some(map) => load_secret_map(
            map,
            &spec.secret_root,
            spec.allow_insecure_secret_permissions,
        )?,
        None => SecretEnvironment(Vec::new()),
    };
    if let (Some(account), Some((uid, gid))) = (&spec.account, spec.uid.zip(spec.gid)) {
        remap_account(account, uid, gid)?;
    }
    if let Some((uid, gid)) = spec.uid.zip(spec.gid) {
        prepare_ownership(&spec.ownership_paths, uid, gid)?;
    }
    if let Some(ssh) = &spec.ssh {
        prepare_ssh_config(ssh, spec.uid.zip(spec.gid))?;
    }
    let mut services = spawn_services(spec)?;
    if let Err(error) = run_prepare_commands(spec, &secrets) {
        terminate_services(&mut services);
        return Err(error);
    }
    let mut command = build_exec_command(spec, &spec.command, &secrets);
    let source = command.exec();
    terminate_services(&mut services);
    Err(InitError::Exec {
        program: spec
            .command
            .program
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
        source,
    })
}

struct ServiceChild {
    child: Child,
    readiness: ServiceReadiness,
}

enum ServiceReadiness {
    Tcp(std::net::SocketAddr),
    Unix(PathBuf),
}

impl ServiceReadiness {
    fn display(&self) -> String {
        match self {
            Self::Tcp(address) => address.to_string(),
            Self::Unix(path) => path.display().to_string(),
        }
    }
}

fn spawn_services(spec: &InitSpec) -> Result<Vec<ServiceChild>, InitError> {
    let executable = std::env::current_exe().map_err(InitError::CurrentExecutable)?;
    let mut children = Vec::with_capacity(spec.services.len());
    for service in &spec.services {
        let (mut command, readiness) = service_command(&executable, service);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let InitServiceSpec::HttpProxy(service) = service {
            // Load the host-owned 0600 bind while init is namespace root, then
            // pass it only to the child created below. It never enters engine
            // configuration, init JSON, argv, or logs.
            let token = AuthToken::from_file(&service.auth_token_file).map_err(|error| {
                InitError::InvalidService(format!(
                    "failed to load HTTP proxy authentication token: {error}"
                ))
            })?;
            command.env(
                HTTP_PROXY_TOKEN_ENV,
                OsString::from_vec(token.expose().to_vec()),
            );
        }
        apply_identity(&mut command, spec.uid.zip(spec.gid));
        let child = command.spawn().map_err(|source| InitError::SpawnService {
            endpoint: readiness.display(),
            source,
        });
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                terminate_services(&mut children);
                return Err(error);
            }
        };
        children.push(ServiceChild { child, readiness });
        if let Err(error) = wait_for_service(&mut children) {
            terminate_services(&mut children);
            return Err(error);
        }
    }
    Ok(children)
}

fn service_command(executable: &Path, service: &InitServiceSpec) -> (Command, ServiceReadiness) {
    match service {
        InitServiceSpec::HttpProxy(service) => http_proxy_service_command(executable, service),
        InitServiceSpec::Connect(service) => connect_service_command(executable, service),
        InitServiceSpec::TcpForward(service) => tcp_forward_service_command(executable, service),
        InitServiceSpec::TcpBridge(service) => tcp_bridge_service_command(executable, service),
        InitServiceSpec::UnixBridge(service) => unix_bridge_service_command(executable, service),
        InitServiceSpec::OauthTarget(service) => oauth_service_command(executable, service),
    }
}

fn http_proxy_service_command(
    executable: &Path,
    service: &HttpProxyServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = Command::new(executable);
    command
        .arg("http-proxy")
        .arg("--listen")
        .arg(service.listen.to_string())
        .arg("--proxy")
        .arg(&service.proxy)
        .arg("--token-env")
        .arg(HTTP_PROXY_TOKEN_ENV)
        .arg("--max-header-bytes")
        .arg(service.max_header_bytes.to_string());
    append_service_limits(
        &mut command,
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    );
    (command, ServiceReadiness::Tcp(service.listen))
}

fn connect_service_command(
    executable: &Path,
    service: &ConnectServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = Command::new(executable);
    command
        .arg("connect-bridge")
        .arg("--listen")
        .arg(service.listen.to_string())
        .arg("--proxy")
        .arg(&service.proxy)
        .arg("--target")
        .arg(&service.target);
    if let Some(token_file) = &service.auth_token_file {
        command.arg("--token-file").arg(token_file);
    }
    append_service_limits(
        &mut command,
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    );
    (command, ServiceReadiness::Tcp(service.listen))
}

fn tcp_forward_service_command(
    executable: &Path,
    service: &TcpForwardServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = Command::new(executable);
    command
        .arg("tcp-forward")
        .arg("--listen")
        .arg(service.listen.to_string())
        .arg("--target")
        .arg(&service.target)
        .arg("--max-connections")
        .arg(service.max_connections.to_string())
        .arg("--connect-timeout-seconds")
        .arg(service.connect_timeout_seconds.to_string())
        .arg("--idle-timeout-seconds")
        .arg(service.idle_timeout_seconds.to_string());
    (command, ServiceReadiness::Tcp(service.listen))
}

fn tcp_bridge_service_command(
    executable: &Path,
    service: &TcpBridgeServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = authenticated_bridge_command(
        executable,
        "tcp-bridge",
        service.listen.to_string(),
        &service.remote,
        &service.token_file,
        service.proxy.as_deref(),
        service.proxy_auth_token_file.as_deref(),
    );
    append_service_limits(
        &mut command,
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    );
    (command, ServiceReadiness::Tcp(service.listen))
}

fn unix_bridge_service_command(
    executable: &Path,
    service: &UnixBridgeServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = authenticated_bridge_command(
        executable,
        "unix-bridge",
        &service.listen,
        &service.remote,
        &service.token_file,
        service.proxy.as_deref(),
        service.proxy_auth_token_file.as_deref(),
    );
    append_service_limits(
        &mut command,
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    );
    (command, ServiceReadiness::Unix(service.listen.clone()))
}

fn authenticated_bridge_command(
    executable: &Path,
    subcommand: &str,
    listen: impl AsRef<std::ffi::OsStr>,
    remote: &str,
    token_file: &Path,
    proxy: Option<&str>,
    proxy_token_file: Option<&Path>,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(subcommand)
        .arg("--listen")
        .arg(listen)
        .arg("--remote")
        .arg(remote)
        .arg("--token-file")
        .arg(token_file);
    if let Some(proxy) = proxy {
        command.arg("--proxy").arg(proxy);
    }
    if let Some(token_file) = proxy_token_file {
        command.arg("--proxy-token-file").arg(token_file);
    }
    command
}

fn oauth_service_command(
    executable: &Path,
    service: &OAuthTargetServiceSpec,
) -> (Command, ServiceReadiness) {
    let mut command = Command::new(executable);
    command
        .arg("oauth-target")
        .arg("--listen")
        .arg(service.listen.to_string())
        .arg("--callback")
        .arg(service.callback.to_string())
        .arg("--token-file")
        .arg(&service.token_file);
    append_service_limits(
        &mut command,
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    );
    (
        command,
        ServiceReadiness::Tcp(readiness_address(service.listen)),
    )
}

fn readiness_address(listen: std::net::SocketAddr) -> std::net::SocketAddr {
    if !listen.ip().is_unspecified() {
        return listen;
    }
    match listen {
        std::net::SocketAddr::V4(address) => {
            std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), address.port())
        }
        std::net::SocketAddr::V6(address) => {
            std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), address.port())
        }
    }
}

fn append_service_limits(
    command: &mut Command,
    max_connections: usize,
    connect_timeout: u64,
    handshake_timeout: u64,
    idle_timeout: u64,
) {
    command
        .arg("--max-connections")
        .arg(max_connections.to_string())
        .arg("--connect-timeout-seconds")
        .arg(connect_timeout.to_string())
        .arg("--handshake-timeout-seconds")
        .arg(handshake_timeout.to_string())
        .arg("--idle-timeout-seconds")
        .arg(idle_timeout.to_string());
}

fn wait_for_service(children: &mut [ServiceChild]) -> Result<(), InitError> {
    use std::os::unix::fs::FileTypeExt;

    for _ in 0..40 {
        let service = children.last_mut().expect("service child was just pushed");
        if let Some(status) = service.child.try_wait().map_err(InitError::ServiceStatus)? {
            return Err(InitError::ServiceExited {
                endpoint: service.readiness.display(),
                status,
            });
        }
        let ready = match &service.readiness {
            ServiceReadiness::Tcp(listen) => tcp_listener_is_bound(*listen),
            ServiceReadiness::Unix(path) => std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_socket()),
        };
        if ready {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(InitError::ServiceReadinessTimeout(
        children
            .last()
            .expect("service child was just pushed")
            .readiness
            .display(),
    ))
}

fn tcp_listener_is_bound(address: std::net::SocketAddr) -> bool {
    std::net::TcpListener::bind(address)
        .is_err_and(|error| error.kind() == io::ErrorKind::AddrInUse)
}

fn terminate_services(children: &mut [ServiceChild]) {
    use std::os::unix::fs::FileTypeExt;

    for service in children {
        let _ = service.child.kill();
        let _ = service.child.wait();
        if let ServiceReadiness::Unix(path) = &service.readiness
            && std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Builds a specification for direct CLI argv, applying the standard secret-map environment.
///
/// # Errors
///
/// Returns an error when argv is empty/non-UTF-8 or an option violates an init
/// invariant.
pub fn direct_spec(
    options: DirectInitOptions,
    command: Vec<OsString>,
) -> Result<InitSpec, InitError> {
    let command = ExecSpec::from_argv(command)?;
    let spec = InitSpec {
        version: 1,
        uid: options.uid,
        gid: options.gid,
        account: None,
        cwd: options.cwd,
        clear_environment: options.clear_environment,
        env: BTreeMap::new(),
        secret_map: options.secret_map,
        secret_root: default_secret_root(),
        allow_insecure_secret_permissions: options.allow_insecure_secret_permissions,
        ownership_paths: options.ownership_paths,
        ssh: options.ssh,
        prepare: options.prepare,
        services: Vec::new(),
        command,
    };
    validate_spec(&spec)?;
    Ok(spec)
}

/// Parses a JSON argv array used by repeatable `--prepare-json` flags.
///
/// # Errors
///
/// Returns an error when input is not a non-empty JSON string array or its
/// program is invalid.
pub fn parse_prepare_json(input: &str) -> Result<CommandSpec, InitError> {
    let argv: Vec<String> = serde_json::from_str(input).map_err(InitError::ParsePrepareArgument)?;
    let mut argv = argv.into_iter();
    let program = argv.next().ok_or(InitError::InvalidCommand)?;
    let command = CommandSpec {
        program,
        args: argv.collect(),
        env: BTreeMap::new(),
        cwd: None,
    };
    validate_command(&command)?;
    Ok(command)
}

/// Initialization errors. Variants intentionally never contain secret values.
#[derive(Debug, Error)]
pub enum InitError {
    #[error("unsupported init specification version {0}")]
    UnsupportedVersion(u32),
    #[error("uid and gid must be supplied together")]
    UidGidPairRequired,
    #[error("an account remap requires uid and gid")]
    AccountRequiresIdentity,
    #[error("invalid workload account name")]
    InvalidAccountName,
    #[error("workload account `{0}` does not exist in the container account database")]
    AccountNotFound(String),
    #[error("workload account `{0}` occurs more than once in the container account database")]
    DuplicateAccount(String),
    #[error("group `{0}` occurs more than once in the container group database")]
    DuplicateGroup(String),
    #[error("target uid {uid} is already assigned to account `{account}`")]
    UidConflict { uid: u32, account: String },
    #[error("invalid account database {path}: {reason}")]
    InvalidAccountDatabase { path: PathBuf, reason: String },
    #[error("failed to update account database {path}: {source}")]
    AccountIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("ownership paths require uid and gid")]
    OwnershipRequiresIdentity,
    #[error("invalid ownership path: {0}")]
    InvalidOwnershipPath(String),
    #[error("ownership traversal exceeded {0} entries")]
    OwnershipLimit(usize),
    #[error("ownership traversal at {path} exceeded depth {maximum}")]
    OwnershipDepth { path: PathBuf, maximum: usize },
    #[error("ownership preparation failed at {path}: {source}")]
    OwnershipIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("final and preparation commands require a non-empty UTF-8 program")]
    InvalidCommand,
    #[error("invalid environment variable name `{0}`")]
    InvalidEnvironmentName(String),
    #[error("invalid CONNECT service: {0}")]
    InvalidService(String),
    #[error("secret root must be an absolute path: {0}")]
    InvalidSecretRoot(PathBuf),
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0} is not a regular file")]
    NotRegularFile(PathBuf),
    #[error("{path} is {length} bytes; maximum is {maximum}")]
    FileTooLarge {
        path: PathBuf,
        length: u64,
        maximum: u64,
    },
    #[error("{path} has insecure mode {mode:o}; group/world access is forbidden")]
    InsecurePermissions { path: PathBuf, mode: u32 },
    #[error("failed to parse init specification {path}: {source}")]
    ParseSpec {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to parse secret map {path}: {source}")]
    ParseSecretMap {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to canonicalize {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("secret mapping `{name}` points outside the configured secret root: {path}")]
    SecretOutsideRoot { name: String, path: PathBuf },
    #[error("secret mapping `{0}` contains a NUL byte")]
    SecretContainsNul(String),
    #[error("unsafe SSH source or destination path: {0}")]
    UnsafeSshPath(PathBuf),
    #[error("failed to prepare SSH configuration at {path}: {source}")]
    PrepareSsh {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to start preparation command `{program}`: {source}")]
    SpawnPrepare {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("preparation command `{program}` exited with {status}")]
    PrepareFailed { program: String, status: ExitStatus },
    #[error("failed to execute final command `{program}`: {source}")]
    Exec {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSON argv for preparation command: {0}")]
    ParsePrepareArgument(#[source] serde_json::Error),
    #[error("failed to locate the init helper executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("failed to start helper service {endpoint}: {source}")]
    SpawnService {
        endpoint: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect loopback service: {0}")]
    ServiceStatus(#[source] io::Error),
    #[error("helper service {endpoint} exited prematurely with {status}")]
    ServiceExited {
        endpoint: String,
        status: ExitStatus,
    },
    #[error("helper service {0} did not become ready")]
    ServiceReadinessTimeout(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn loads_secret_map_under_root_and_redacts_debug() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("secrets");
        std::fs::create_dir(&root).unwrap();
        let secret = root.join("api-key");
        std::fs::write(&secret, b"highly-secret\n").unwrap();
        chmod(&secret, 0o600);
        let map = directory.path().join("map.json");
        std::fs::write(
            &map,
            serde_json::to_vec(&BTreeMap::from([("API_KEY", &secret)])).unwrap(),
        )
        .unwrap();
        chmod(&map, 0o600);
        let loaded = load_secret_map(&map, &root, false).unwrap();
        assert_eq!(loaded.len(), 1);
        let debug = format!("{loaded:?}");
        assert!(debug.contains("API_KEY"));
        assert!(!debug.contains("highly-secret"));
    }

    #[test]
    fn rejects_outside_symlink_bad_permissions_and_nul() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("secrets");
        std::fs::create_dir(&root).unwrap();
        let outside = directory.path().join("outside");
        std::fs::write(&outside, b"secret").unwrap();
        chmod(&outside, 0o600);
        let linked = root.join("linked");
        symlink(&outside, &linked).unwrap();
        let map = directory.path().join("map.json");
        std::fs::write(
            &map,
            serde_json::to_vec(&BTreeMap::from([("KEY", &linked)])).unwrap(),
        )
        .unwrap();
        chmod(&map, 0o600);
        assert!(matches!(
            load_secret_map(&map, &root, false),
            Err(InitError::SecretOutsideRoot { .. })
        ));

        let insecure = root.join("insecure");
        std::fs::write(&insecure, b"secret").unwrap();
        chmod(&insecure, 0o644);
        std::fs::write(
            &map,
            serde_json::to_vec(&BTreeMap::from([("KEY", &insecure)])).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load_secret_map(&map, &root, false),
            Err(InitError::InsecurePermissions { .. })
        ));

        let nul = root.join("nul");
        std::fs::write(&nul, b"bad\0value").unwrap();
        chmod(&nul, 0o600);
        std::fs::write(
            &map,
            serde_json::to_vec(&BTreeMap::from([("KEY", &nul)])).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load_secret_map(&map, &root, false),
            Err(InitError::SecretContainsNul(_))
        ));
    }

    #[test]
    fn ssh_setup_copies_only_supported_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let home = directory.path().join("home");
        let destination = home.join(".ssh");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(source.join("config"), b"Host *\n").unwrap();
        std::fs::write(source.join("id_rsa"), b"must not copy").unwrap();
        prepare_ssh_config(
            &SshSetup {
                source,
                destination: destination.clone(),
            },
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(destination.join("config")).unwrap(),
            b"Host *\n"
        );
        assert!(!destination.join("id_rsa").exists());
        assert_eq!(
            std::fs::metadata(&destination).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(destination.join("config"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn parses_prepare_as_argv_not_shell() {
        let command = parse_prepare_json(r#"["printf","$(not-executed)"]"#).unwrap();
        assert_eq!(command.program, "printf");
        assert_eq!(command.args, ["$(not-executed)"]);
        assert!(parse_prepare_json("[]").is_err());
        assert!(parse_prepare_json(r#"{"program":"sh"}"#).is_err());
    }

    #[test]
    fn spec_rejects_unknown_fields_and_invalid_identity() {
        let bad = br#"{
            "version": 1,
            "uid": 1000,
            "gid": null,
            "command": {"program": "codex", "args": [], "env": {}, "cwd": null},
            "unknown": true
        }"#;
        assert!(serde_json::from_slice::<InitSpec>(bad).is_err());
    }

    #[test]
    fn preparation_reports_nonzero_exit() {
        let spec = InitSpec {
            version: 1,
            uid: None,
            gid: None,
            account: None,
            cwd: None,
            clear_environment: false,
            env: BTreeMap::new(),
            secret_map: None,
            secret_root: default_secret_root(),
            allow_insecure_secret_permissions: false,
            ownership_paths: Vec::new(),
            ssh: None,
            prepare: vec![CommandSpec {
                program: "/usr/bin/false".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            }],
            services: Vec::new(),
            command: ExecSpec {
                program: "/usr/bin/true".to_owned().into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            },
        };
        assert!(matches!(
            run_prepare_commands(&spec, &SecretEnvironment(Vec::new())),
            Err(InitError::PrepareFailed { .. })
        ));
    }

    #[test]
    fn final_argv_round_trips_non_utf8_bytes() {
        let original = vec![
            OsString::from("codex"),
            OsString::from_vec(vec![b'f', 0x80, b'o']),
        ];
        let command = ExecSpec::from_argv(original.clone()).unwrap();
        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains("unix_base64"));
        assert!(!json.contains('�'));
        let decoded: ExecSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.argv(), original);
    }

    #[test]
    fn remaps_existing_account_and_group_with_atomic_permission_preserving_files() {
        let directory = tempfile::tempdir().unwrap();
        let passwd = directory.path().join("passwd");
        let group = directory.path().join("group");
        std::fs::write(
            &passwd,
            b"root:x:0:0:root:/root:/bin/sh\ncodex:x:1000:1000:Codex:/home/codex:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(&group, b"root:x:0:\ncodex:x:1000:\n").unwrap();
        chmod(&passwd, 0o640);
        chmod(&group, 0o644);
        let old_passwd_inode = std::fs::metadata(&passwd).unwrap().ino();
        let old_group_inode = std::fs::metadata(&group).unwrap().ino();

        remap_account_files(&passwd, &group, "codex", 4242, 4343).unwrap();

        assert_eq!(
            std::fs::read(&passwd).unwrap(),
            b"root:x:0:0:root:/root:/bin/sh\ncodex:x:4242:4343:Codex:/home/codex:/bin/sh\n"
        );
        assert_eq!(
            std::fs::read(&group).unwrap(),
            b"root:x:0:\ncodex:x:4343:\n"
        );
        assert_eq!(std::fs::metadata(&passwd).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(std::fs::metadata(&group).unwrap().mode() & 0o7777, 0o644);
        assert_ne!(std::fs::metadata(&passwd).unwrap().ino(), old_passwd_inode);
        assert_ne!(std::fs::metadata(&group).unwrap().ino(), old_group_inode);
    }

    #[test]
    fn account_remap_rejects_uid_conflict_without_changing_either_database() {
        let directory = tempfile::tempdir().unwrap();
        let passwd = directory.path().join("passwd");
        let group = directory.path().join("group");
        let passwd_bytes =
            b"codex:x:1000:1000::/home/codex:/bin/sh\nother:x:4242:4242::/home/other:/bin/sh\n";
        let group_bytes = b"codex:x:1000:\nother:x:4242:\n";
        std::fs::write(&passwd, passwd_bytes).unwrap();
        std::fs::write(&group, group_bytes).unwrap();

        assert!(matches!(
            remap_account_files(&passwd, &group, "codex", 4242, 4343),
            Err(InitError::UidConflict { uid: 4242, .. })
        ));
        assert_eq!(std::fs::read(&passwd).unwrap(), passwd_bytes);
        assert_eq!(std::fs::read(&group).unwrap(), group_bytes);
    }

    #[test]
    fn account_remap_leaves_same_named_group_when_target_gid_is_taken() {
        let directory = tempfile::tempdir().unwrap();
        let passwd = directory.path().join("passwd");
        let group = directory.path().join("group");
        std::fs::write(
            &passwd,
            b"codex:x:1000:1000::/home/codex:/bin/sh\nother:x:2000:20::/home/other:/bin/sh\n",
        )
        .unwrap();
        let group_bytes = b"codex:x:1000:\nstaff:x:20:\n";
        std::fs::write(&group, group_bytes).unwrap();
        let old_group_inode = std::fs::metadata(&group).unwrap().ino();

        remap_account_files(&passwd, &group, "codex", 4242, 20).unwrap();

        assert!(
            std::fs::read_to_string(&passwd)
                .unwrap()
                .contains("codex:x:4242:20:")
        );
        assert_eq!(std::fs::read(&group).unwrap(), group_bytes);
        assert_eq!(std::fs::metadata(&group).unwrap().ino(), old_group_inode);
    }

    #[test]
    fn validates_account_names_and_requires_a_target_identity() {
        for invalid in ["", "-codex", "name:with-colon", "name with space", "cødex"] {
            assert!(matches!(
                validate_account_name(invalid),
                Err(InitError::InvalidAccountName)
            ));
        }
        assert!(validate_account_name("codex-start_1").is_ok());

        let mut spec = direct_spec(
            DirectInitOptions::default(),
            vec![OsString::from("/usr/bin/true")],
        )
        .unwrap();
        spec.account = Some("codex".to_owned());
        assert!(matches!(
            validate_spec(&spec),
            Err(InitError::AccountRequiresIdentity)
        ));
    }

    #[test]
    fn ownership_is_bounded_to_real_directory_and_skips_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let root = directory_path.join("owned");
        let outside = directory_path.join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), b"data").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let metadata = std::fs::metadata(&root).unwrap();
        let report = prepare_ownership(&[root], metadata.uid(), metadata.gid()).unwrap();
        assert_eq!(report.changed, 2);
        assert_eq!(report.skipped_symlinks, 1);
        assert_eq!(report.skipped_filesystems, 0);
    }

    #[test]
    fn ownership_creates_missing_roots_before_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let root = directory_path.join("state/codex-start");
        let metadata = std::fs::metadata(&directory_path).unwrap();

        let report =
            prepare_ownership(std::slice::from_ref(&root), metadata.uid(), metadata.gid()).unwrap();

        assert!(root.is_dir());
        assert_eq!(report.changed, 1);
    }

    #[test]
    fn ownership_does_not_create_roots_through_symlinked_parents() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let directory_path = directory.path().canonicalize().unwrap();
        let outside_path = outside.path().canonicalize().unwrap();
        let linked = directory_path.join("linked");
        symlink(&outside_path, &linked).unwrap();
        let root = linked.join("codex-start");
        let metadata = std::fs::metadata(&directory_path).unwrap();

        assert!(matches!(
            prepare_ownership(&[root], metadata.uid(), metadata.gid()),
            Err(InitError::InvalidOwnershipPath(_))
        ));
        assert!(!outside_path.join("codex-start").exists());
    }

    #[test]
    fn ownership_rejects_system_relative_and_duplicate_roots() {
        for roots in [
            vec![PathBuf::from("/")],
            vec![PathBuf::from("relative")],
            vec![PathBuf::from("/tmp/a/../b")],
            vec![PathBuf::from("/etc/codex-start")],
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/a")],
        ] {
            assert!(validate_ownership_paths(&roots).is_err());
        }
        assert!(
            validate_ownership_paths(&[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/a/child")])
                .is_ok()
        );
    }

    #[test]
    fn tcp_readiness_probe_does_not_open_a_connection() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(tcp_listener_is_bound(address));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn tagged_services_round_trip_and_validate_strictly() {
        let connect = InitServiceSpec::Connect(ConnectServiceSpec {
            listen: "127.0.0.1:11434".parse().unwrap(),
            proxy: "egress:3128".to_owned(),
            target: "host.docker.internal:11434".to_owned(),
            auth_token_file: None,
            max_connections: 64,
            connect_timeout_seconds: 10,
            handshake_timeout_seconds: 10,
            idle_timeout_seconds: 300,
        });
        let json = serde_json::to_string(&connect).unwrap();
        assert!(json.contains(r#""kind":"connect""#));
        assert_eq!(
            serde_json::from_str::<InitServiceSpec>(&json).unwrap(),
            connect
        );

        let direct = TcpForwardServiceSpec {
            listen: "127.0.0.1:1234".parse().unwrap(),
            target: "host.docker.internal:1234".to_owned(),
            max_connections: 64,
            connect_timeout_seconds: 10,
            idle_timeout_seconds: 300,
        };
        assert!(validate_tcp_forward_service(&direct).is_ok());

        let tcp_bridge = InitServiceSpec::TcpBridge(TcpBridgeServiceSpec {
            listen: "127.0.0.1:11434".parse().unwrap(),
            remote: "host.docker.internal:49152".to_owned(),
            token_file: PathBuf::from("/run/codex-start/secrets/host-relay-token"),
            proxy: Some("egress:3128".to_owned()),
            proxy_auth_token_file: Some(PathBuf::from("/run/codex-start/secrets/proxy-token")),
            max_connections: 64,
            connect_timeout_seconds: 10,
            handshake_timeout_seconds: 5,
            idle_timeout_seconds: 300,
        });
        let InitServiceSpec::TcpBridge(tcp_bridge_spec) = &tcp_bridge else {
            unreachable!();
        };
        assert!(validate_tcp_bridge_service(tcp_bridge_spec, &default_secret_root()).is_ok());
        let bridge_json = serde_json::to_string(&tcp_bridge).unwrap();
        assert!(bridge_json.contains(r#""kind":"tcp-bridge""#));
        assert_eq!(
            serde_json::from_str::<InitServiceSpec>(&bridge_json).unwrap(),
            tcp_bridge
        );
        let (bridge_command, readiness) =
            service_command(Path::new("/usr/local/bin/codex-start-init"), &tcp_bridge);
        assert!(
            bridge_command
                .get_args()
                .any(|argument| argument == "tcp-bridge")
        );
        assert!(matches!(readiness, ServiceReadiness::Tcp(_)));

        let invalid_tcp_bridge = TcpBridgeServiceSpec {
            listen: "0.0.0.0:11434".parse().unwrap(),
            ..tcp_bridge_spec.clone()
        };
        assert!(validate_tcp_bridge_service(&invalid_tcp_bridge, &default_secret_root()).is_err());

        let unix = UnixBridgeServiceSpec {
            listen: PathBuf::from("/tmp/codex-start/ssh-agent.sock"),
            remote: "host.docker.internal:22022".to_owned(),
            token_file: PathBuf::from("/run/codex-start/secrets/ssh-agent-token"),
            proxy: Some("egress:3128".to_owned()),
            proxy_auth_token_file: None,
            max_connections: 64,
            connect_timeout_seconds: 10,
            handshake_timeout_seconds: 5,
            idle_timeout_seconds: 300,
        };
        assert!(validate_unix_service(&unix, &default_secret_root()).is_ok());
        let unsafe_service = UnixBridgeServiceSpec {
            listen: PathBuf::from("/etc/ssh-agent.sock"),
            ..unix
        };
        assert!(validate_unix_service(&unsafe_service, &default_secret_root()).is_err());

        let oauth = OAuthTargetServiceSpec {
            listen: "0.0.0.0:1455".parse().unwrap(),
            callback: "127.0.0.1:1455".parse().unwrap(),
            token_file: PathBuf::from("/run/codex-start/secrets/oauth-token"),
            max_connections: 64,
            connect_timeout_seconds: 10,
            handshake_timeout_seconds: 5,
            idle_timeout_seconds: 300,
        };
        assert!(validate_oauth_service(&oauth, &default_secret_root()).is_ok());
        assert_eq!(
            readiness_address(oauth.listen),
            "127.0.0.1:1455".parse().unwrap()
        );
        let unsafe_oauth = OAuthTargetServiceSpec {
            callback: "0.0.0.0:1455".parse().unwrap(),
            ..oauth
        };
        assert!(validate_oauth_service(&unsafe_oauth, &default_secret_root()).is_err());
    }
}
