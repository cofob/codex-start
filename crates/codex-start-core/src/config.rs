//! Strict, schema-versioned and provenance-aware configuration resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current on-disk configuration schema.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;
/// Prefix used for nested environment overrides.
pub const ENVIRONMENT_PREFIX: &str = "CODEX_START__";

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    #[default]
    Auto,
    Docker,
    Podman,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    Offline,
    #[default]
    Allowlist,
    Bridge,
    Host,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtyMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAgentBridge {
    #[default]
    Auto,
    Socket,
    Tcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeKind {
    Managed,
    Host,
    Path,
}

/// A complete named Codex home definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HomeConfig {
    Managed {
        #[serde(default)]
        name: Option<String>,
    },
    Host,
    Path {
        path: PathBuf,
        #[serde(default)]
        agents_path: Option<PathBuf>,
    },
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self::Managed {
            name: Some("default".into()),
        }
    }
}

impl HomeConfig {
    #[must_use]
    pub const fn kind(&self) -> HomeKind {
        match self {
            Self::Managed { .. } => HomeKind::Managed,
            Self::Host => HomeKind::Host,
            Self::Path { .. } => HomeKind::Path,
        }
    }

    fn validate(&self, key: &str) -> Result<(), ConfigError> {
        match self {
            Self::Managed { name } => {
                if name.as_ref().is_some_and(|name| name.trim().is_empty()) {
                    return Err(ConfigError::Invalid(format!(
                        "managed home `{key}` has an empty storage name"
                    )));
                }
            }
            Self::Host => {}
            Self::Path { path, agents_path } => {
                if !path.is_absolute() {
                    return Err(ConfigError::Invalid(format!(
                        "home `{key}` path must be absolute"
                    )));
                }
                if agents_path.as_ref().is_some_and(|path| !path.is_absolute()) {
                    return Err(ConfigError::Invalid(format!(
                        "home `{key}` agents_path must be absolute"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretProviderKind {
    Environment,
    File,
    Command,
    Keychain,
}

/// A named secret source. Secret values are intentionally not representable here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretProvider {
    Environment { variable: String },
    File { path: PathBuf },
    Command { argv: Vec<String> },
    Keychain { service: String, account: String },
}

impl SecretProvider {
    #[must_use]
    pub const fn kind(&self) -> SecretProviderKind {
        match self {
            Self::Environment { .. } => SecretProviderKind::Environment,
            Self::File { .. } => SecretProviderKind::File,
            Self::Command { .. } => SecretProviderKind::Command,
            Self::Keychain { .. } => SecretProviderKind::Keychain,
        }
    }

    fn validate(&self, key: &str) -> Result<(), ConfigError> {
        match self {
            Self::Environment { variable } if !valid_env_name(variable) => {
                Err(ConfigError::Invalid(format!(
                    "secret `{key}` has invalid environment variable `{variable}`"
                )))
            }
            Self::File { path } if !path.is_absolute() => Err(ConfigError::Invalid(format!(
                "secret `{key}` file path must be absolute"
            ))),
            Self::Command { argv } if argv.is_empty() || argv[0].trim().is_empty() => Err(
                ConfigError::Invalid(format!("secret `{key}` command argv cannot be empty")),
            ),
            Self::Command { argv } if argv.iter().any(|arg| arg.contains('\0')) => Err(
                ConfigError::Invalid(format!("secret `{key}` command contains a NUL byte")),
            ),
            Self::Keychain { service, account }
                if service.trim().is_empty() || account.trim().is_empty() =>
            {
                Err(ConfigError::Invalid(format!(
                    "secret `{key}` keychain service and account cannot be empty"
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Partial forwarding settings used in a configuration layer.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardingPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_agent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_agent_bridge: Option<SshAgentBridge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpg_agent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_config: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_hosts: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_ssh: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gh_config: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_providers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_opener: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_ssh_program: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_callback_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_config_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_hosts_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_ssh_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingConfig {
    pub ssh_agent: bool,
    pub ssh_agent_bridge: SshAgentBridge,
    pub gpg_agent: bool,
    pub git_config: bool,
    pub known_hosts: bool,
    pub host_ssh: bool,
    pub gh_config: bool,
    pub browser: bool,
    pub local_providers: bool,
    pub browser_opener: Vec<String>,
    pub host_ssh_program: PathBuf,
    pub oauth_callback_port: u16,
    pub git_config_file: Option<PathBuf>,
    pub known_hosts_file: Option<PathBuf>,
    pub container_ssh_dir: PathBuf,
    pub ssh_user: Option<String>,
}

impl Default for ForwardingConfig {
    fn default() -> Self {
        Self {
            ssh_agent: true,
            ssh_agent_bridge: SshAgentBridge::Auto,
            gpg_agent: true,
            git_config: true,
            known_hosts: true,
            host_ssh: true,
            gh_config: true,
            browser: true,
            local_providers: true,
            browser_opener: Vec::new(),
            host_ssh_program: PathBuf::from("ssh"),
            oauth_callback_port: 1455,
            git_config_file: None,
            known_hosts_file: None,
            container_ssh_dir: PathBuf::from("/home/codex/.ssh"),
            ssh_user: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_base: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    pub worktree_base: Option<PathBuf>,
    pub branch_prefix: String,
    pub editor: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            worktree_base: None,
            branch_prefix: "codex/".into(),
            editor: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_private_addresses: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_header_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub listen_port: u16,
    pub connect_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub max_connections: usize,
    pub block_private_addresses: bool,
    pub header_timeout_seconds: u64,
    pub max_header_bytes: usize,
    pub handshake_timeout_seconds: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 3128,
            connect_timeout_seconds: 10,
            idle_timeout_seconds: 300,
            max_connections: 256,
            block_private_addresses: true,
            header_timeout_seconds: 10,
            max_header_bytes: 65_536,
            handshake_timeout_seconds: 5,
        }
    }
}

/// Native Codex configuration. `config` accepts all current and future Codex keys.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    pub profile: Option<String>,
    pub args: Vec<String>,
    /// Arbitrary native Codex TOML configuration.
    pub config: BTreeMap<String, toml::Value>,
}

impl CodexConfig {
    /// Render native configuration as deterministic `-c key=value` arguments.
    #[must_use]
    pub fn override_args(&self) -> Vec<String> {
        let mut flattened = Vec::new();
        flatten_codex_table(None, &self.config, &mut flattened);
        flattened
            .into_iter()
            .flat_map(|(key, value)| ["-c".to_owned(), format!("{key}={value}")])
            .collect()
    }

    /// Generated configuration arguments followed by user arguments, preserving order.
    #[must_use]
    pub fn command_args(&self) -> Vec<String> {
        let mut result = self.override_args();
        if let Some(profile) = &self.profile {
            result.extend(["--profile".into(), profile.clone()]);
        }
        result.extend(self.args.iter().cloned());
        result
    }

    /// Resolve the native MCP OAuth listener and public callback base URL.
    ///
    /// `override_expressions` must be in their eventual command-line order and
    /// contain only the values supplied to Codex's `-c`/`--config` options.
    /// These expressions have higher precedence than [`Self::config`].
    pub fn mcp_oauth_callback(
        &self,
        fallback_port: u16,
        override_expressions: &[String],
    ) -> Result<McpOauthCallback, ConfigError> {
        let mut values = CallbackValues::default();
        if let Some(value) = self.config.get(MCP_OAUTH_CALLBACK_PORT) {
            values.port = Some((
                value.clone(),
                format!("codex.config.{MCP_OAUTH_CALLBACK_PORT}"),
            ));
        }
        if let Some(value) = self.config.get(MCP_OAUTH_CALLBACK_URL) {
            values.url = Some((
                value.clone(),
                format!("codex.config.{MCP_OAUTH_CALLBACK_URL}"),
            ));
        }
        for (index, expression) in override_expressions.iter().enumerate() {
            values.apply_expression(expression, index)?;
        }
        McpOauthCallback::resolve(fallback_port, &values)
    }
}

const MCP_OAUTH_CALLBACK_PORT: &str = "mcp_oauth_callback_port";
const MCP_OAUTH_CALLBACK_URL: &str = "mcp_oauth_callback_url";

/// Coherent native Codex MCP OAuth callback settings for one launch.
///
/// The URL is restricted to plaintext loopback because `codex-start` owns a
/// local reverse TCP tunnel rather than a public TLS ingress. Codex appends its
/// server-specific callback identifier to this base URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpOauthCallback {
    listener: std::net::SocketAddr,
    base_url: String,
    generate_port: bool,
    generate_url: bool,
}

impl McpOauthCallback {
    /// Construct callback settings using the canonical IPv4 loopback URL.
    pub fn from_port(port: u16) -> Result<Self, ConfigError> {
        Self::resolve(port, &CallbackValues::default())
    }

    /// Host loopback address represented by the browser-visible base URL.
    #[must_use]
    pub const fn listener(&self) -> std::net::SocketAddr {
        self.listener
    }

    /// In-container callback listener controlled by Codex's native port.
    ///
    /// Codex owns an IPv4 loopback HTTP listener independently of a custom
    /// ingress/base URL, so the reverse tunnel always targets this address.
    #[must_use]
    pub fn codex_listener(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            self.listener.port(),
        )
    }

    /// Browser-visible base URL supplied to native Codex OAuth handling.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Native Codex overrides required to complete missing user settings.
    ///
    /// The caller must place these before user configuration arguments, so an
    /// explicit, already-coordinated user value retains normal Codex precedence.
    #[must_use]
    pub fn generated_override_args(&self) -> Vec<String> {
        let mut result = Vec::with_capacity(usize::from(self.generate_port) * 2 + 2);
        if self.generate_port {
            result.extend([
                "-c".to_owned(),
                format!("{MCP_OAUTH_CALLBACK_PORT}={}", self.listener.port()),
            ]);
        }
        if self.generate_url {
            result.extend([
                "-c".to_owned(),
                format!(
                    "{MCP_OAUTH_CALLBACK_URL}={}",
                    toml_literal(&toml::Value::String(self.base_url.clone()))
                ),
            ]);
        }
        result
    }

    fn resolve(fallback_port: u16, values: &CallbackValues) -> Result<Self, ConfigError> {
        let explicit_port = values
            .port
            .as_ref()
            .map(|(value, path)| callback_port(value, path))
            .transpose()?;
        let explicit_url = values
            .url
            .as_ref()
            .map(|(value, path)| callback_url(value, path))
            .transpose()?;
        let (port, ip, base_url) = match (explicit_port, explicit_url.as_ref()) {
            (Some(port), Some(url)) if port != url.port => {
                return Err(ConfigError::Invalid(format!(
                    "native Codex MCP OAuth callback URL port {} conflicts with {MCP_OAUTH_CALLBACK_PORT}={port}",
                    url.port
                )));
            }
            (Some(port), Some(url)) => (port, url.ip, url.value.clone()),
            (Some(port), None) => (
                port,
                std::net::Ipv4Addr::LOCALHOST.into(),
                canonical_callback_url(port),
            ),
            (None, Some(url)) => (url.port, url.ip, url.value.clone()),
            (None, None) => {
                if fallback_port == 0 {
                    return Err(invalid_callback_port("forwarding.oauth_callback_port"));
                }
                (
                    fallback_port,
                    std::net::Ipv4Addr::LOCALHOST.into(),
                    canonical_callback_url(fallback_port),
                )
            }
        };
        Ok(Self {
            listener: std::net::SocketAddr::new(ip, port),
            base_url,
            generate_port: values.port.is_none(),
            generate_url: values.url.is_none(),
        })
    }
}

#[derive(Default)]
struct CallbackValues {
    port: Option<(toml::Value, String)>,
    url: Option<(toml::Value, String)>,
}

impl CallbackValues {
    fn apply_expression(&mut self, expression: &str, index: usize) -> Result<(), ConfigError> {
        let Some((raw_key, raw_value)) = expression.split_once('=') else {
            return Ok(());
        };
        let Some(key) = callback_override_key(raw_key.trim()) else {
            return Ok(());
        };
        let value = codex_override_value(raw_value.trim());
        let path = format!("Codex config override {} ({key})", index + 1);
        match key {
            MCP_OAUTH_CALLBACK_PORT => self.port = Some((value, path)),
            MCP_OAUTH_CALLBACK_URL => self.url = Some((value, path)),
            _ => {
                return Err(ConfigError::Internal(format!(
                    "unhandled MCP OAuth callback setting {key}"
                )));
            }
        }
        Ok(())
    }
}

struct ParsedCallbackUrl {
    value: String,
    ip: std::net::IpAddr,
    port: u16,
}

fn callback_port(value: &toml::Value, path: &str) -> Result<u16, ConfigError> {
    value
        .as_integer()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid_callback_port(path))
}

fn invalid_callback_port(path: &str) -> ConfigError {
    ConfigError::Invalid(format!(
        "native Codex MCP OAuth callback setting `{path}` must be an integer from 1 through 65535"
    ))
}

fn callback_url(value: &toml::Value, path: &str) -> Result<ParsedCallbackUrl, ConfigError> {
    let raw = value.as_str().ok_or_else(|| invalid_callback_url(path))?;
    if raw.trim() != raw || raw.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(invalid_callback_url(path));
    }
    let url = url::Url::parse(raw).map_err(|_| invalid_callback_url(path))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_callback_url(path));
    }
    let ip = match url.host() {
        Some(url::Host::Domain(host)) if host.eq_ignore_ascii_case("localhost") => {
            std::net::Ipv4Addr::LOCALHOST.into()
        }
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => ip.into(),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => ip.into(),
        _ => return Err(invalid_callback_url(path)),
    };
    let port = url
        .port_or_known_default()
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid_callback_url(path))?;
    Ok(ParsedCallbackUrl {
        value: raw.to_owned(),
        ip,
        port,
    })
}

fn invalid_callback_url(path: &str) -> ConfigError {
    ConfigError::Invalid(format!(
        "native Codex MCP OAuth callback setting `{path}` must be an HTTP loopback base URL without credentials, query, or fragment"
    ))
}

fn canonical_callback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn callback_override_key(raw: &str) -> Option<&'static str> {
    if matches!(raw, MCP_OAUTH_CALLBACK_PORT | MCP_OAUTH_CALLBACK_URL) {
        return match raw {
            MCP_OAUTH_CALLBACK_PORT => Some(MCP_OAUTH_CALLBACK_PORT),
            MCP_OAUTH_CALLBACK_URL => Some(MCP_OAUTH_CALLBACK_URL),
            _ => None,
        };
    }
    let document = format!("{raw}=true").parse::<toml::Table>().ok()?;
    if document.len() != 1 {
        return None;
    }
    match document.keys().next().map(String::as_str) {
        Some(MCP_OAUTH_CALLBACK_PORT) => Some(MCP_OAUTH_CALLBACK_PORT),
        Some(MCP_OAUTH_CALLBACK_URL) => Some(MCP_OAUTH_CALLBACK_URL),
        _ => None,
    }
}

fn codex_override_value(raw: &str) -> toml::Value {
    let input = format!("value={raw}");
    if let Ok(mut document) = input.parse::<toml::Table>()
        && document.len() == 1
        && let Some(value) = document.remove("value")
    {
        value
    } else {
        toml::Value::String(raw.to_owned())
    }
}

/// A partial settings layer. Options distinguish omission from an explicit value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<TtyMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_hosts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_ssh_hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_refs: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forwarding: Option<ForwardingPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexPatch>,
}

impl ConfigPatch {
    #[must_use]
    pub fn built_in_defaults() -> Self {
        Self {
            environment: Some("generic".into()),
            runtime: Some(RuntimeKind::Auto),
            network: Some(NetworkMode::Allowlist),
            worktree: Some(WorktreeMode::Auto),
            home: Some("default".into()),
            rebuild: Some(false),
            tty: Some(TtyMode::Auto),
            forwarding: Some(ForwardingPatch {
                ssh_agent: Some(true),
                ssh_agent_bridge: Some(SshAgentBridge::Auto),
                gpg_agent: Some(true),
                git_config: Some(true),
                known_hosts: Some(true),
                host_ssh: Some(true),
                gh_config: Some(true),
                browser: Some(true),
                local_providers: Some(true),
                browser_opener: Some(Vec::new()),
                host_ssh_program: Some(PathBuf::from("ssh")),
                oauth_callback_port: Some(1455),
                git_config_file: None,
                known_hosts_file: None,
                container_ssh_dir: Some(PathBuf::from("/home/codex/.ssh")),
                ssh_user: None,
            }),
            git: Some(GitPatch {
                worktree_base: None,
                branch_prefix: Some("codex/".into()),
                editor: Some(Vec::new()),
            }),
            proxy: Some(ProxyPatch {
                listen_port: Some(3128),
                connect_timeout_seconds: Some(10),
                idle_timeout_seconds: Some(300),
                max_connections: Some(256),
                block_private_addresses: Some(true),
                header_timeout_seconds: Some(10),
                max_header_bytes: Some(65_536),
                handshake_timeout_seconds: Some(5),
            }),
            codex: Some(CodexPatch {
                config: BTreeMap::from([
                    (
                        "sandbox_mode".into(),
                        toml::Value::String("danger-full-access".into()),
                    ),
                    (
                        "approval_policy".into(),
                        toml::Value::String("on-request".into()),
                    ),
                ]),
                ..CodexPatch::default()
            }),
            ..Self::default()
        }
    }

    /// Validate one partial layer without resolving it.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_patch(self)
    }

    /// Deep-merge a higher-precedence partial layer into this one.
    ///
    /// Maps merge recursively and arrays replace, matching full configuration
    /// resolution semantics.
    pub fn merged_with(&self, overlay: &Self) -> Result<Self, ConfigError> {
        self.validate()?;
        overlay.validate()?;
        let mut destination = toml::Value::try_from(self)
            .map_err(|error| ConfigError::Internal(error.to_string()))?;
        let source = toml::Value::try_from(overlay)
            .map_err(|error| ConfigError::Internal(error.to_string()))?;
        merge_value(
            &mut destination,
            source,
            "",
            &ValueSource {
                kind: ConfigLayerKind::Environment,
                label: "environment inheritance".into(),
            },
            &mut BTreeMap::new(),
        );
        destination
            .try_into()
            .map_err(|error: toml::de::Error| ConfigError::Internal(error.to_string()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    pub settings: ConfigPatch,
}

/// Strict top-level schema. Project documents may contain only `settings`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDocument {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub settings: ConfigPatch,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub homes: BTreeMap<String, HomeConfig>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretProvider>,
}

const fn schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            settings: ConfigPatch::default(),
            profiles: BTreeMap::new(),
            homes: BTreeMap::new(),
            secrets: BTreeMap::new(),
        }
    }
}

impl ConfigDocument {
    pub fn parse(input: &str, source: impl Into<String>) -> Result<Self, ConfigError> {
        let source = source.into();
        let document = toml::from_str::<Self>(input).map_err(|error| {
            let message = error.to_string();
            ConfigError::Parse {
                source_label: source,
                suggestion: unknown_field_suggestion(&message),
                message,
                span: error.span(),
            }
        })?;
        document.validate()?;
        Ok(document)
    }

    pub fn parse_file(path: &Path, input: &str) -> Result<Self, ConfigError> {
        Self::parse(input, path.display().to_string())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema {
                found: self.schema_version,
                supported: CONFIG_SCHEMA_VERSION,
            });
        }
        for (name, home) in &self.homes {
            validate_name("home", name)?;
            home.validate(name)?;
        }
        for (name, secret) in &self.secrets {
            validate_name("secret", name)?;
            secret.validate(name)?;
        }
        for name in self.profiles.keys() {
            validate_name("profile", name)?;
        }
        validate_patch(&self.settings)?;
        for profile in self.profiles.values() {
            validate_patch(&profile.settings)?;
        }
        Ok(())
    }

    pub fn validate_as_project(&self) -> Result<(), ConfigError> {
        self.validate()?;
        if !self.profiles.is_empty() || !self.homes.is_empty() || !self.secrets.is_empty() {
            return Err(ConfigError::ProjectDefinitions);
        }
        Ok(())
    }
}

/// Final settings after all layers have been resolved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfig {
    pub schema_version: u32,
    pub selected_profile: Option<String>,
    pub environment: String,
    pub runtime: RuntimeKind,
    pub network: NetworkMode,
    pub worktree: WorktreeMode,
    pub home_name: String,
    pub home: HomeConfig,
    pub name: Option<String>,
    pub publish: Vec<String>,
    pub rebuild: bool,
    pub tty: TtyMode,
    pub workdir: Option<PathBuf>,
    pub allow_hosts: Vec<String>,
    pub allow_ssh_hosts: Vec<String>,
    pub secret_refs: BTreeMap<String, String>,
    pub forwarding: ForwardingConfig,
    pub git: GitConfig,
    pub proxy: ProxyConfig,
    pub codex: CodexConfig,
    pub homes: BTreeMap<String, HomeConfig>,
    #[serde(skip_serializing, default)]
    pub secrets: BTreeMap<String, SecretProvider>,
}

/// Layer precedence. Ordering is the documented lowest-to-highest precedence.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLayerKind {
    BuiltIn,
    Environment,
    Global,
    Profile,
    Project,
    EnvironmentVariables,
    CommandLine,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueSource {
    pub kind: ConfigLayerKind,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigLayer {
    pub kind: ConfigLayerKind,
    pub source: ValueSource,
    pub patch: ConfigPatch,
}

impl ConfigLayer {
    #[must_use]
    pub fn new(kind: ConfigLayerKind, label: impl Into<String>, patch: ConfigPatch) -> Self {
        Self {
            kind,
            source: ValueSource {
                kind,
                label: label.into(),
            },
            patch,
        }
    }
}

/// Origin of every final leaf field, addressable by dotted path.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    fields: BTreeMap<String, ValueSource>,
}

impl Provenance {
    #[must_use]
    pub fn source_for(&self, path: &str) -> Option<&ValueSource> {
        self.fields.get(path).or_else(|| {
            let mut candidate = path;
            while let Some((parent, _)) = candidate.rsplit_once('.') {
                if let Some(source) = self.fields.get(parent) {
                    return Some(source);
                }
                candidate = parent;
            }
            None
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ValueSource)> {
        self.fields
            .iter()
            .map(|(path, source)| (path.as_str(), source))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedConfig {
    pub config: EffectiveConfig,
    pub provenance: Provenance,
    pub layers: Vec<ValueSource>,
}

/// Builder and resolver for all documented configuration precedence levels.
#[derive(Clone, Debug)]
pub struct ConfigResolver {
    layers: Vec<ConfigLayer>,
    profiles: BTreeMap<String, ProfileConfig>,
    homes: BTreeMap<String, HomeConfig>,
    home_sources: BTreeMap<String, ValueSource>,
    secrets: BTreeMap<String, SecretProvider>,
}

impl Default for ConfigResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigResolver {
    #[must_use]
    pub fn new() -> Self {
        let built_in_source = ValueSource {
            kind: ConfigLayerKind::BuiltIn,
            label: "built-in defaults".into(),
        };
        Self {
            layers: vec![ConfigLayer::new(
                ConfigLayerKind::BuiltIn,
                built_in_source.label.clone(),
                ConfigPatch::built_in_defaults(),
            )],
            profiles: BTreeMap::new(),
            homes: BTreeMap::from([("default".into(), HomeConfig::default())]),
            home_sources: BTreeMap::from([("default".into(), built_in_source)]),
            secrets: BTreeMap::new(),
        }
    }

    pub fn add_layer(&mut self, layer: ConfigLayer) -> Result<&mut Self, ConfigError> {
        if layer.kind == ConfigLayerKind::Profile {
            return Err(ConfigError::ReservedProfileLayer);
        }
        validate_patch(&layer.patch)?;
        self.layers.push(layer);
        Ok(self)
    }

    pub fn add_document(
        &mut self,
        kind: ConfigLayerKind,
        label: impl Into<String>,
        document: ConfigDocument,
    ) -> Result<&mut Self, ConfigError> {
        let source = ValueSource {
            kind,
            label: label.into(),
        };
        document.validate()?;
        if kind == ConfigLayerKind::Project {
            document.validate_as_project()?;
        } else if !matches!(kind, ConfigLayerKind::Global | ConfigLayerKind::BuiltIn)
            && (!document.profiles.is_empty()
                || !document.homes.is_empty()
                || !document.secrets.is_empty())
        {
            return Err(ConfigError::DefinitionsOutsideGlobal);
        }
        for (name, profile) in document.profiles {
            self.profiles.insert(name, profile);
        }
        for (name, home) in document.homes {
            self.home_sources.insert(name.clone(), source.clone());
            self.homes.insert(name, home);
        }
        self.secrets.extend(document.secrets);
        self.add_layer(ConfigLayer {
            kind,
            source,
            patch: document.settings,
        })
    }

    /// Add all `CODEX_START__...` values as one high-precedence typed patch.
    pub fn add_environment_overrides<I, K, V>(
        &mut self,
        values: I,
    ) -> Result<&mut Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let patch = environment_patch(values)?;
        if patch != ConfigPatch::default() {
            self.add_layer(ConfigLayer::new(
                ConfigLayerKind::EnvironmentVariables,
                ENVIRONMENT_PREFIX,
                patch,
            ))?;
        }
        Ok(self)
    }

    pub fn resolve(mut self) -> Result<ResolvedConfig, ConfigError> {
        for (name, home) in &self.homes {
            home.validate(name)?;
        }
        for (name, secret) in &self.secrets {
            secret.validate(name)?;
        }
        self.layers.sort_by_key(|layer| layer.kind);
        let selected_profile = self
            .layers
            .iter()
            .filter_map(|layer| layer.patch.profile.as_ref())
            .next_back()
            .cloned();
        if let Some(profile) = &selected_profile {
            let mut profile_layers = Vec::new();
            collect_profile_layers(
                profile,
                &self.profiles,
                &mut Vec::new(),
                &mut profile_layers,
            )?;
            self.layers
                .extend(profile_layers.into_iter().map(|(name, patch)| {
                    ConfigLayer::new(ConfigLayerKind::Profile, format!("profile:{name}"), patch)
                }));
            self.layers.sort_by_key(|layer| layer.kind);
        }

        let mut merged = toml::Value::Table(toml::map::Map::new());
        let mut provenance = Provenance::default();
        for layer in &self.layers {
            let value = toml::Value::try_from(&layer.patch)
                .map_err(|error| ConfigError::Internal(error.to_string()))?;
            merge_value(
                &mut merged,
                value,
                "",
                &layer.source,
                &mut provenance.fields,
            );
        }
        for (name, home) in &self.homes {
            let Some(source) = self.home_sources.get(name) else {
                continue;
            };
            let value = toml::Value::try_from(home)
                .map_err(|error| ConfigError::Internal(error.to_string()))?;
            record_leaves(
                &value,
                &format!("homes.{name}"),
                source,
                &mut provenance.fields,
            );
        }
        let patch = merged
            .try_into::<ConfigPatch>()
            .map_err(|error| ConfigError::Internal(error.to_string()))?;
        let config = effective_from_patch(patch, selected_profile, self.homes, self.secrets)?;
        validate_effective(&config)?;
        Ok(ResolvedConfig {
            config,
            provenance,
            layers: self.layers.into_iter().map(|layer| layer.source).collect(),
        })
    }
}

/// Parse nested `CODEX_START__` variables into a strict partial settings document.
pub fn environment_patch<I, K, V>(values: I) -> Result<ConfigPatch, ConfigError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut root = toml::map::Map::new();
    for (key, value) in values {
        let key = key.as_ref();
        let Some(path) = key.strip_prefix(ENVIRONMENT_PREFIX) else {
            continue;
        };
        let segments = path
            .split("__")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if segments.is_empty() {
            return Err(ConfigError::InvalidEnvironmentOverride(key.into()));
        }
        let parsed = parse_override_value(value.as_ref());
        insert_toml_path(&mut root, &segments, parsed)
            .map_err(|()| ConfigError::ConflictingEnvironmentOverride(key.into()))?;
    }
    toml::Value::Table(root)
        .try_into::<ConfigPatch>()
        .map_err(|error| ConfigError::EnvironmentOverride(error.to_string()))
}

fn parse_override_value(value: &str) -> toml::Value {
    let wrapper = format!("value = {value}");
    toml::from_str::<toml::Value>(&wrapper)
        .ok()
        .and_then(|value| value.get("value").cloned())
        .unwrap_or_else(|| toml::Value::String(value.to_owned()))
}

fn insert_toml_path(
    table: &mut toml::map::Map<String, toml::Value>,
    segments: &[String],
    value: toml::Value,
) -> Result<(), ()> {
    let Some((head, tail)) = segments.split_first() else {
        return Err(());
    };
    if tail.is_empty() {
        if table.contains_key(head) {
            return Err(());
        }
        table.insert(head.clone(), value);
        return Ok(());
    }
    let entry = table
        .entry(head.clone())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let toml::Value::Table(child) = entry else {
        return Err(());
    };
    insert_toml_path(child, tail, value)
}

fn collect_profile_layers(
    name: &str,
    profiles: &BTreeMap<String, ProfileConfig>,
    stack: &mut Vec<String>,
    output: &mut Vec<(String, ConfigPatch)>,
) -> Result<(), ConfigError> {
    if let Some(position) = stack.iter().position(|item| item == name) {
        let mut cycle = stack[position..].to_vec();
        cycle.push(name.to_owned());
        return Err(ConfigError::ProfileCycle(cycle.join(" -> ")));
    }
    let profile = profiles
        .get(name)
        .ok_or_else(|| ConfigError::UnknownProfile(name.to_owned()))?;
    stack.push(name.to_owned());
    if let Some(parent) = &profile.extends {
        collect_profile_layers(parent, profiles, stack, output)?;
    }
    stack.pop();
    output.push((name.to_owned(), profile.settings.clone()));
    Ok(())
}

#[cfg(test)]
fn merge_patches(base: &ConfigPatch, overlay: &ConfigPatch) -> Result<ConfigPatch, ConfigError> {
    base.merged_with(overlay)
}

fn merge_value(
    destination: &mut toml::Value,
    source: toml::Value,
    path: &str,
    value_source: &ValueSource,
    provenance: &mut BTreeMap<String, ValueSource>,
) {
    if let (toml::Value::Table(destination_table), toml::Value::Table(source_table)) =
        (&mut *destination, &source)
    {
        for (key, value) in source_table {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(existing) = destination_table.get_mut(key) {
                merge_value(
                    existing,
                    value.clone(),
                    &child_path,
                    value_source,
                    provenance,
                );
            } else {
                record_leaves(value, &child_path, value_source, provenance);
                destination_table.insert(key.clone(), value.clone());
            }
        }
        return;
    }
    remove_provenance_subtree(path, provenance);
    record_leaves(&source, path, value_source, provenance);
    *destination = source;
}

fn remove_provenance_subtree(path: &str, provenance: &mut BTreeMap<String, ValueSource>) {
    let child_prefix = format!("{path}.");
    provenance.retain(|candidate, _| candidate != path && !candidate.starts_with(&child_prefix));
}

fn record_leaves(
    value: &toml::Value,
    path: &str,
    source: &ValueSource,
    provenance: &mut BTreeMap<String, ValueSource>,
) {
    if let toml::Value::Table(table) = value {
        for (key, value) in table {
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            record_leaves(value, &child, source, provenance);
        }
    } else {
        provenance.insert(path.to_owned(), source.clone());
    }
}

fn effective_from_patch(
    patch: ConfigPatch,
    selected_profile: Option<String>,
    homes: BTreeMap<String, HomeConfig>,
    secrets: BTreeMap<String, SecretProvider>,
) -> Result<EffectiveConfig, ConfigError> {
    let home_name = patch.home.unwrap_or_else(|| "default".into());
    let home = homes
        .get(&home_name)
        .cloned()
        .ok_or_else(|| ConfigError::UnknownHome(home_name.clone()))?;
    let forwarding = patch.forwarding.unwrap_or_default();
    let git = patch.git.unwrap_or_default();
    let proxy = patch.proxy.unwrap_or_default();
    let codex = patch.codex.unwrap_or_default();
    Ok(EffectiveConfig {
        schema_version: CONFIG_SCHEMA_VERSION,
        selected_profile,
        environment: patch.environment.unwrap_or_else(|| "generic".into()),
        runtime: patch.runtime.unwrap_or_default(),
        network: patch.network.unwrap_or_default(),
        worktree: patch.worktree.unwrap_or_default(),
        home_name,
        home,
        name: patch.name,
        publish: patch.publish.unwrap_or_default(),
        rebuild: patch.rebuild.unwrap_or(false),
        tty: patch.tty.unwrap_or_default(),
        workdir: patch.workdir,
        allow_hosts: patch.allow_hosts.unwrap_or_default(),
        allow_ssh_hosts: patch.allow_ssh_hosts.unwrap_or_default(),
        secret_refs: patch.secret_refs,
        forwarding: ForwardingConfig {
            ssh_agent: forwarding.ssh_agent.unwrap_or(true),
            ssh_agent_bridge: forwarding.ssh_agent_bridge.unwrap_or_default(),
            gpg_agent: forwarding.gpg_agent.unwrap_or(true),
            git_config: forwarding.git_config.unwrap_or(true),
            known_hosts: forwarding.known_hosts.unwrap_or(true),
            host_ssh: forwarding.host_ssh.unwrap_or(true),
            gh_config: forwarding.gh_config.unwrap_or(true),
            browser: forwarding.browser.unwrap_or(true),
            local_providers: forwarding.local_providers.unwrap_or(true),
            browser_opener: forwarding.browser_opener.unwrap_or_default(),
            host_ssh_program: forwarding
                .host_ssh_program
                .unwrap_or_else(|| PathBuf::from("ssh")),
            oauth_callback_port: forwarding.oauth_callback_port.unwrap_or(1455),
            git_config_file: forwarding.git_config_file,
            known_hosts_file: forwarding.known_hosts_file,
            container_ssh_dir: forwarding
                .container_ssh_dir
                .unwrap_or_else(|| PathBuf::from("/home/codex/.ssh")),
            ssh_user: forwarding.ssh_user,
        },
        git: GitConfig {
            worktree_base: git.worktree_base,
            branch_prefix: git.branch_prefix.unwrap_or_else(|| "codex/".into()),
            editor: git.editor.unwrap_or_default(),
        },
        proxy: ProxyConfig {
            listen_port: proxy.listen_port.unwrap_or(3128),
            connect_timeout_seconds: proxy.connect_timeout_seconds.unwrap_or(10),
            idle_timeout_seconds: proxy.idle_timeout_seconds.unwrap_or(300),
            max_connections: proxy.max_connections.unwrap_or(256),
            block_private_addresses: proxy.block_private_addresses.unwrap_or(true),
            header_timeout_seconds: proxy.header_timeout_seconds.unwrap_or(10),
            max_header_bytes: proxy.max_header_bytes.unwrap_or(65_536),
            handshake_timeout_seconds: proxy.handshake_timeout_seconds.unwrap_or(5),
        },
        codex: CodexConfig {
            profile: codex.profile,
            args: codex.args.unwrap_or_default(),
            config: codex.config,
        },
        homes,
        secrets,
    })
}

fn validate_patch(patch: &ConfigPatch) -> Result<(), ConfigError> {
    if patch
        .environment
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(ConfigError::Invalid("environment cannot be empty".into()));
    }
    if patch
        .home
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(ConfigError::Invalid("home cannot be empty".into()));
    }
    if patch
        .name
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(ConfigError::Invalid("run name cannot be empty".into()));
    }
    if patch
        .workdir
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(ConfigError::Invalid("workdir must be absolute".into()));
    }
    validate_host_rules("allow_hosts", patch.allow_hosts.as_deref())?;
    validate_host_rules("allow_ssh_hosts", patch.allow_ssh_hosts.as_deref())?;
    if let Some(git) = &patch.git {
        if git.branch_prefix.as_ref().is_some_and(|value| {
            value.is_empty() || !value.ends_with('/') || value.starts_with('/')
        }) {
            return Err(ConfigError::Invalid(
                "git.branch_prefix must be relative and end with '/'".into(),
            ));
        }
        if git.editor.as_ref().is_some_and(Vec::is_empty) {
            // An empty editor intentionally means automatic discovery.
        }
    }
    if let Some(proxy) = &patch.proxy {
        validate_proxy_patch(proxy)?;
    }
    if let Some(forwarding) = &patch.forwarding {
        validate_forwarding_patch(forwarding)?;
    }
    for (target, secret) in &patch.secret_refs {
        if !valid_env_name(target) || secret.trim().is_empty() {
            return Err(ConfigError::Invalid(format!(
                "secret reference `{target}` must use an environment-variable target and a non-empty provider name"
            )));
        }
    }
    if let Some(codex) = &patch.codex {
        validate_codex_secret_literals(&codex.config, "codex.config")?;
    }
    Ok(())
}

fn validate_proxy_patch(proxy: &ProxyPatch) -> Result<(), ConfigError> {
    if proxy.listen_port == Some(0)
        || proxy.connect_timeout_seconds == Some(0)
        || proxy.idle_timeout_seconds == Some(0)
        || proxy.max_connections == Some(0)
        || proxy.header_timeout_seconds == Some(0)
        || proxy.max_header_bytes == Some(0)
        || proxy.handshake_timeout_seconds == Some(0)
    {
        return Err(ConfigError::Invalid(
            "proxy listen_port, timeouts, and max_connections must be non-zero".into(),
        ));
    }
    Ok(())
}

fn validate_forwarding_patch(forwarding: &ForwardingPatch) -> Result<(), ConfigError> {
    if forwarding.oauth_callback_port == Some(0) {
        return Err(ConfigError::Invalid(
            "forwarding.oauth_callback_port must be non-zero".into(),
        ));
    }
    if forwarding
        .host_ssh_program
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(ConfigError::Invalid(
            "forwarding.host_ssh_program cannot be empty".into(),
        ));
    }
    for (name, path) in [
        ("git_config_file", forwarding.git_config_file.as_ref()),
        ("known_hosts_file", forwarding.known_hosts_file.as_ref()),
    ] {
        if path.is_some_and(|path| {
            !path.is_absolute()
                && !path
                    .to_str()
                    .is_some_and(|value| value == "~" || value.starts_with("~/"))
        }) {
            return Err(ConfigError::Invalid(format!(
                "forwarding.{name} must be absolute or start with ~/"
            )));
        }
    }
    if forwarding.container_ssh_dir.as_ref().is_some_and(|path| {
        !path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || !path.starts_with("/home/codex")
    }) {
        return Err(ConfigError::Invalid(
            "forwarding.container_ssh_dir must be an absolute path below /home/codex".into(),
        ));
    }
    if forwarding.ssh_user.as_ref().is_some_and(|user| {
        user.is_empty()
            || user.len() > 255
            || !user.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'+' | b'-')
            })
    }) {
        return Err(ConfigError::Invalid(
            "forwarding.ssh_user contains unsupported characters".into(),
        ));
    }
    Ok(())
}

fn validate_host_rules(field: &str, rules: Option<&[String]>) -> Result<(), ConfigError> {
    for rule in rules.unwrap_or_default() {
        if !valid_host_rule_syntax(rule) {
            return Err(ConfigError::Invalid(format!(
                "{field} contains invalid authority rule `{rule}`"
            )));
        }
    }
    Ok(())
}

pub(crate) fn valid_host_rule_syntax(rule: &str) -> bool {
    if rule.is_empty()
        || rule.contains(['/', '@', '?', '#'])
        || rule
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    let (wildcard, candidate) = rule
        .strip_prefix("*.")
        .map_or((false, rule), |candidate| (true, candidate));
    let Some(host) = host_rule_name(candidate) else {
        return false;
    };
    if wildcard && host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    valid_dns_or_ip(host)
}

fn host_rule_name(candidate: &str) -> Option<&str> {
    if let Some(remainder) = candidate.strip_prefix('[') {
        let (host, suffix) = remainder.split_once(']')?;
        if !matches!(
            host.parse::<std::net::IpAddr>(),
            Ok(std::net::IpAddr::V6(_))
        ) || !valid_optional_port(suffix)
        {
            return None;
        }
        return Some(host);
    }
    if candidate.parse::<std::net::Ipv6Addr>().is_ok() {
        return Some(candidate);
    }
    if candidate.contains(['[', ']']) {
        return None;
    }
    match candidate.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && valid_port_matcher(port) => Some(host),
        Some(_) => None,
        None => Some(candidate),
    }
}

fn valid_optional_port(suffix: &str) -> bool {
    suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port_matcher)
}

fn valid_port_matcher(value: &str) -> bool {
    value == "*"
        || !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.parse::<u16>().is_ok_and(|port| port != 0)
}

pub(crate) fn valid_dns_or_ip(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let Ok(ascii) = idna::domain_to_ascii_strict(host) else {
        return false;
    };
    !ascii.is_empty()
        && ascii.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn validate_codex_secret_literals(
    table: &BTreeMap<String, toml::Value>,
    prefix: &str,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let path = format!("{prefix}.{key}");
        validate_codex_secret_value(key, value, &path, HeaderMode::Regular)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderMode {
    Regular,
    Static,
    Environment,
}

fn validate_codex_secret_value(
    key: &str,
    value: &toml::Value,
    path: &str,
    mode: HeaderMode,
) -> Result<(), ConfigError> {
    if mode == HeaderMode::Static {
        return Err(ConfigError::Invalid(format!(
            "native Codex static HTTP header `{path}` is not allowed; map the header name to a global-provider environment variable with `env_http_headers` instead"
        )));
    }
    if mode == HeaderMode::Environment {
        if value.as_str().is_some_and(valid_env_name) {
            return Ok(());
        }
        return Err(ConfigError::Invalid(format!(
            "native Codex environment-header setting `{path}` must name an environment variable"
        )));
    }
    if mode == HeaderMode::Regular && secret_environment_name_key(key) {
        if value.as_str().is_some_and(valid_env_name) {
            return Ok(());
        }
        return Err(ConfigError::Invalid(format!(
            "native Codex secret environment setting `{path}` must name an environment variable"
        )));
    }
    // Secret-bearing native settings are strings.  Do not reject numeric or
    // boolean Codex controls merely because their future-facing name contains
    // a word such as `token` (for example `tool_output_token_limit`).
    if mode == HeaderMode::Regular
        && secret_literal_key(key)
        && matches!(value, toml::Value::String(_))
    {
        return Err(plaintext_codex_secret(path));
    }

    let child_mode = header_mode(key);
    match value {
        toml::Value::Table(child) => {
            for (child_key, child_value) in child {
                let child_path = format!("{path}.{child_key}");
                validate_codex_secret_value(child_key, child_value, &child_path, child_mode)?;
            }
        }
        toml::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                validate_codex_array_value(child, &format!("{path}[{index}]"), child_mode)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_codex_array_value(
    value: &toml::Value,
    path: &str,
    mode: HeaderMode,
) -> Result<(), ConfigError> {
    if mode == HeaderMode::Static {
        return Err(ConfigError::Invalid(format!(
            "native Codex static HTTP header `{path}` is not allowed; use `env_http_headers` instead"
        )));
    }
    if mode == HeaderMode::Environment {
        return if value.as_str().is_some_and(valid_env_name) {
            Ok(())
        } else {
            Err(ConfigError::Invalid(format!(
                "native Codex environment-header setting `{path}` must name an environment variable"
            )))
        };
    }
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                validate_codex_secret_value(key, value, &format!("{path}.{key}"), mode)?;
            }
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_codex_array_value(value, &format!("{path}[{index}]"), mode)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn plaintext_codex_secret(path: &str) -> ConfigError {
    ConfigError::Invalid(format!(
        "native Codex setting `{path}` looks like a plaintext secret; reference a global provider and an environment-variable Codex setting instead"
    ))
}

fn header_mode(key: &str) -> HeaderMode {
    match normalized_secret_key(key).as_str() {
        "http_headers" | "headers" => HeaderMode::Static,
        "env_http_headers" | "http_headers_env" => HeaderMode::Environment,
        _ => HeaderMode::Regular,
    }
}

fn secret_literal_key(key: &str) -> bool {
    let key = normalized_secret_key(key);
    !environment_name_key(&key) && raw_secret_key(&key)
}

fn secret_environment_name_key(key: &str) -> bool {
    let key = normalized_secret_key(key);
    environment_name_key(&key) && raw_secret_key(&key)
}

fn environment_name_key(key: &str) -> bool {
    key.contains("env_var")
        || key.contains("environment_variable")
        || key == "env_key"
        || key.ends_with("_env")
}

fn raw_secret_key(key: &str) -> bool {
    [
        "api_key",
        "access_key",
        "access_token",
        "auth_token",
        "authorization",
        "bearer_token",
        "client_secret",
        "cookie",
        "credential",
        "credentials",
        "passphrase",
        "password",
        "pat",
        "private_key",
        "proxy_authorization",
        "secret",
        "token",
    ]
    .iter()
    .any(|phrase| contains_secret_phrase(key, phrase))
}

fn contains_secret_phrase(key: &str, phrase: &str) -> bool {
    key == phrase
        || key.starts_with(&format!("{phrase}_"))
        || key.ends_with(&format!("_{phrase}"))
        || key.contains(&format!("_{phrase}_"))
}

fn normalized_secret_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len());
    let mut separator = false;
    for character in key.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
            separator = false;
        } else if !normalized.is_empty() && !separator {
            normalized.push('_');
            separator = true;
        }
    }
    normalized.trim_end_matches('_').to_owned()
}

fn validate_effective(config: &EffectiveConfig) -> Result<(), ConfigError> {
    for (target, reference) in &config.secret_refs {
        if !config.secrets.contains_key(reference) {
            return Err(ConfigError::UnknownSecret {
                target: target.clone(),
                name: reference.clone(),
            });
        }
    }
    if config.network == NetworkMode::Host && !config.publish.is_empty() {
        return Err(ConfigError::Invalid(
            "published ports cannot be combined with host networking".into(),
        ));
    }
    Ok(())
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
        })
}

fn validate_name(kind: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(ConfigError::Invalid(format!(
            "{kind} name `{value}` must contain 1-64 lowercase ASCII letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

fn flatten_codex_table(
    prefix: Option<&str>,
    table: &BTreeMap<String, toml::Value>,
    output: &mut Vec<(String, String)>,
) {
    for (key, value) in table {
        let segment = toml_key_segment(key);
        let key = prefix.map_or_else(|| segment.clone(), |prefix| format!("{prefix}.{segment}"));
        if let toml::Value::Table(child) = value {
            let child = child
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>();
            flatten_codex_table(Some(&key), &child, output);
        } else {
            output.push((key, toml_literal(value)));
        }
    }
}

fn toml_key_segment(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        key.to_owned()
    } else {
        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_owned())
    }
}

fn toml_literal(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
        }
        _ => value.to_string(),
    }
}

fn unknown_field_suggestion(message: &str) -> Option<String> {
    const FIELDS: &[&str] = &[
        "schema_version",
        "settings",
        "profiles",
        "homes",
        "secrets",
        "profile",
        "environment",
        "runtime",
        "network",
        "worktree",
        "home",
        "name",
        "publish",
        "rebuild",
        "tty",
        "workdir",
        "allow_hosts",
        "allow_ssh_hosts",
        "secret_refs",
        "forwarding",
        "git",
        "proxy",
        "codex",
        "extends",
        "kind",
        "path",
        "agents_path",
        "args",
        "variable",
        "argv",
        "service",
        "account",
        "config",
        "ssh_agent",
        "ssh_agent_bridge",
        "gpg_agent",
        "git_config",
        "known_hosts",
        "host_ssh",
        "gh_config",
        "browser",
        "browser_opener",
        "host_ssh_program",
        "oauth_callback_port",
        "git_config_file",
        "known_hosts_file",
        "container_ssh_dir",
        "ssh_user",
        "local_providers",
        "worktree_base",
        "branch_prefix",
        "editor",
        "connect_timeout_seconds",
        "listen_port",
        "idle_timeout_seconds",
        "max_connections",
        "block_private_addresses",
        "header_timeout_seconds",
        "max_header_bytes",
        "handshake_timeout_seconds",
    ];
    let marker = "unknown field `";
    let start = message.find(marker)? + marker.len();
    let end = message[start..].find('`')? + start;
    let unknown = &message[start..end];
    FIELDS
        .iter()
        .map(|candidate| (*candidate, edit_distance(unknown, candidate)))
        .min_by_key(|(_, distance)| *distance)
        .filter(|(_, distance)| *distance <= 3)
        .map(|(candidate, _)| candidate.to_owned())
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.chars().enumerate() {
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_char != right_char)),
            );
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to parse configuration {source_label}: {message}{suggestion_text}", suggestion_text = suggestion.as_ref().map(|value| format!("; did you mean `{value}`?")).unwrap_or_default())]
    Parse {
        source_label: String,
        message: String,
        span: Option<std::ops::Range<usize>>,
        suggestion: Option<String>,
    },
    #[error("unsupported configuration schema {found}; this version supports {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("profiles, homes, and secrets are not allowed in project configuration")]
    ProjectDefinitions,
    #[error("profiles, homes, and secrets may be declared only in global configuration")]
    DefinitionsOutsideGlobal,
    #[error("profile `{0}` is not defined")]
    UnknownProfile(String),
    #[error("profile inheritance cycle: {0}")]
    ProfileCycle(String),
    #[error("home `{0}` is not defined")]
    UnknownHome(String),
    #[error("secret `{name}` referenced by `{target}` is not defined globally")]
    UnknownSecret { target: String, name: String },
    #[error("profile layers are synthesized by the resolver and cannot be added directly")]
    ReservedProfileLayer,
    #[error("invalid environment override `{0}`")]
    InvalidEnvironmentOverride(String),
    #[error("environment override `{0}` conflicts with another override")]
    ConflictingEnvironmentOverride(String),
    #[error("invalid environment overrides: {0}")]
    EnvironmentOverride(String),
    #[error("internal configuration conversion failed: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn strict_document_reports_unknown_field_and_suggestion() {
        let error = ConfigDocument::parse(
            "schema_version=1\n[settings]\nruntim='docker'\n",
            "project.toml",
        )
        .expect_err("typo must fail");
        match error {
            ConfigError::Parse {
                source_label,
                span,
                suggestion,
                ..
            } => {
                assert_eq!(source_label, "project.toml");
                assert!(span.is_some());
                assert_eq!(suggestion.as_deref(), Some("runtime"));
            }
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn launcher_host_rules_reject_malformed_dns_idna_and_authorities() {
        for field in ["allow_hosts", "allow_ssh_hosts"] {
            for rule in [
                "bad_label.example",
                "-leading.example",
                "trailing-.example",
                "empty..label",
                "*.127.0.0.1",
                "example.com:0",
                "example.com:not-a-port",
                "[not-ipv6]:443",
                "https://example.com",
            ] {
                let input = format!("[settings]\n{field}=[{rule:?}]");
                assert!(ConfigDocument::parse(&input, "config.toml").is_err());
            }
            for rule in [
                "faß.de",
                "*.example.com:*",
                "127.0.0.1:443",
                "[2001:db8::1]:443",
            ] {
                let input = format!("[settings]\n{field}=[{rule:?}]");
                assert!(ConfigDocument::parse(&input, "config.toml").is_ok());
            }
        }
    }

    #[test]
    fn native_codex_static_headers_cannot_persist_literals() {
        for header in [
            "Authorization",
            "Cookie",
            "Proxy-Authorization",
            "X-Api-Key",
            "X-Auth-Token",
            "X-Custom-Secret",
            "X-Client-Value",
        ] {
            let input = format!(
                r#"
                [settings.codex.config.mcp_servers.docs]
                url = "https://mcp.example.test"
                [settings.codex.config.mcp_servers.docs.http_headers]
                {header:?} = "plaintext"
                "#
            );
            assert!(ConfigDocument::parse(&input, "config.toml").is_err());
        }
    }

    #[test]
    fn native_codex_environment_header_names_and_token_env_vars_are_allowed() {
        let input = r#"
            [settings.codex.config.mcp_servers.docs]
            url = "https://mcp.example.test"
            bearer_token_env_var = "DOCS_MCP_TOKEN"
            [settings.codex.config.mcp_servers.docs.env_http_headers]
            Authorization = "DOCS_AUTHORIZATION"
            Cookie = "DOCS_COOKIE"
            X-Api-Key = "DOCS_API_KEY"
            X-Client-Value = "DOCS_CLIENT_VALUE"
        "#;
        assert!(ConfigDocument::parse(input, "config.toml").is_ok());

        let invalid = r#"
            [settings.codex.config.mcp_servers.docs.env_http_headers]
            Authorization = "Bearer plaintext"
        "#;
        assert!(ConfigDocument::parse(invalid, "config.toml").is_err());

        let invalid_token_env = r#"
            [settings.codex.config.mcp_servers.docs]
            bearer_token_env_var = "Bearer plaintext"
        "#;
        assert!(ConfigDocument::parse(invalid_token_env, "config.toml").is_err());
    }

    #[test]
    fn native_codex_token_named_resource_limits_remain_future_compatible() {
        let input = r"
            [settings.codex.config]
            tool_output_token_limit = 16384
            model_auto_compact_token_limit = 64000
            token_budget = 4096
        ";
        let document = ConfigDocument::parse(input, "config.toml").expect("numeric controls");
        assert_eq!(
            document.settings.codex.expect("Codex settings").config["token_budget"].as_integer(),
            Some(4096)
        );

        let plaintext = r#"
            [settings.codex.config]
            bearer_token = "still-not-allowed"
        "#;
        assert!(ConfigDocument::parse(plaintext, "config.toml").is_err());
    }

    #[test]
    fn resolution_obeys_precedence_and_tracks_leaf_sources() {
        let global = ConfigDocument::parse(
            r#"
            [settings]
            runtime = "docker"
            environment = "generic"
            allow_hosts = ["global.example"]
            [settings.codex.config]
            model = "global-model"
            model_reasoning_effort = "medium"
            [profiles.work.settings]
            environment = "rust"
            [profiles.work.settings.codex.config]
            model_reasoning_effort = "high"
            "#,
            "global",
        )
        .expect("global");
        let project = ConfigDocument::parse(
            r#"
            [settings]
            profile = "work"
            runtime = "podman"
            allow_hosts = ["project.example"]
            [settings.codex.config]
            model = "project-model"
            "#,
            "project",
        )
        .expect("project");
        let mut resolver = ConfigResolver::new();
        resolver
            .add_document(ConfigLayerKind::Global, "global", global)
            .expect("add global")
            .add_document(ConfigLayerKind::Project, "project", project)
            .expect("add project")
            .add_environment_overrides([("CODEX_START__RUNTIME", "\"docker\"")])
            .expect("add env")
            .add_layer(ConfigLayer::new(
                ConfigLayerKind::CommandLine,
                "command line",
                ConfigPatch {
                    network: Some(NetworkMode::Offline),
                    ..ConfigPatch::default()
                },
            ))
            .expect("add cli");
        let result = resolver.resolve().expect("resolve");
        assert_eq!(result.config.environment, "rust");
        assert_eq!(result.config.runtime, RuntimeKind::Docker);
        assert_eq!(result.config.network, NetworkMode::Offline);
        assert_eq!(result.config.allow_hosts, ["project.example"]);
        assert_eq!(
            result.config.codex.config["model"].as_str(),
            Some("project-model")
        );
        assert_eq!(
            result.config.codex.config["model_reasoning_effort"].as_str(),
            Some("high")
        );
        assert_eq!(
            result
                .provenance
                .source_for("codex.config.model")
                .expect("source")
                .label,
            "project"
        );
        assert_eq!(
            result
                .provenance
                .source_for("runtime")
                .expect("source")
                .kind,
            ConfigLayerKind::EnvironmentVariables
        );
    }

    #[test]
    fn environment_overrides_support_nested_arbitrary_codex_values() {
        let patch = environment_patch([
            ("IGNORED", "yes"),
            ("CODEX_START__REBUILD", "true"),
            ("CODEX_START__CODEX__ARGS", "['exec', '--json']"),
            ("CODEX_START__CODEX__CONFIG__MODEL", "gpt-5.4"),
            ("CODEX_START__PROXY__MAX_CONNECTIONS", "64"),
        ])
        .expect("patch");
        assert_eq!(patch.rebuild, Some(true));
        assert_eq!(
            patch.codex.as_ref().and_then(|codex| codex.args.as_ref()),
            Some(&vec!["exec".into(), "--json".into()])
        );
        assert_eq!(
            patch
                .codex
                .as_ref()
                .and_then(|codex| codex.config["model"].as_str()),
            Some("gpt-5.4")
        );
        assert_eq!(
            patch.proxy.and_then(|proxy| proxy.max_connections),
            Some(64)
        );
    }

    #[test]
    fn arrays_replace_and_maps_deep_merge() {
        let base = ConfigPatch {
            publish: Some(vec!["1000:1000".into(), "2000:2000".into()]),
            codex: Some(CodexPatch {
                config: BTreeMap::from([
                    ("model".into(), toml::Value::String("old".into())),
                    ("effort".into(), toml::Value::String("high".into())),
                ]),
                ..CodexPatch::default()
            }),
            ..ConfigPatch::default()
        };
        let overlay = ConfigPatch {
            publish: Some(vec!["3000:3000".into()]),
            codex: Some(CodexPatch {
                config: BTreeMap::from([("model".into(), toml::Value::String("new".into()))]),
                ..CodexPatch::default()
            }),
            ..ConfigPatch::default()
        };
        let merged = merge_patches(&base, &overlay).expect("merge");
        assert_eq!(merged.publish, Some(vec!["3000:3000".into()]));
        let config = merged.codex.expect("codex").config;
        assert_eq!(config["model"].as_str(), Some("new"));
        assert_eq!(config["effort"].as_str(), Some("high"));
    }

    #[test]
    fn replacing_a_table_removes_stale_descendant_provenance() {
        let mut destination: toml::Value =
            toml::from_str("[codex.config.value]\nnested=1").expect("destination");
        let source: toml::Value = toml::from_str("[codex.config]\nvalue='scalar'").expect("source");
        let mut provenance = BTreeMap::from([(
            "codex.config.value.nested".into(),
            ValueSource {
                kind: ConfigLayerKind::Global,
                label: "old".into(),
            },
        )]);
        merge_value(
            &mut destination,
            source,
            "",
            &ValueSource {
                kind: ConfigLayerKind::Project,
                label: "new".into(),
            },
            &mut provenance,
        );
        assert!(!provenance.contains_key("codex.config.value.nested"));
        assert_eq!(provenance["codex.config.value"].label, "new");
    }

    #[test]
    fn profiles_inherit_and_cycles_are_rejected() {
        let mut resolver = ConfigResolver::new();
        resolver
            .add_document(
                ConfigLayerKind::Global,
                "global",
                ConfigDocument::parse(
                    r#"
                    [settings]
                    profile = "child"
                    [profiles.parent.settings]
                    runtime = "docker"
                    [profiles.child]
                    extends = "parent"
                    [profiles.child.settings]
                    environment = "rust"
                    "#,
                    "global",
                )
                .expect("parse"),
            )
            .expect("add");
        let result = resolver.resolve().expect("resolve");
        assert_eq!(result.config.runtime, RuntimeKind::Docker);
        assert_eq!(result.config.environment, "rust");
        assert_eq!(
            result
                .provenance
                .source_for("runtime")
                .expect("runtime")
                .label,
            "profile:parent"
        );
        assert_eq!(
            result
                .provenance
                .source_for("environment")
                .expect("environment")
                .label,
            "profile:child"
        );

        let mut cyclic = ConfigResolver::new();
        cyclic
            .add_document(
                ConfigLayerKind::Global,
                "global",
                ConfigDocument::parse(
                    r#"
                    [settings]
                    profile = "a"
                    [profiles.a]
                    extends = "b"
                    [profiles.b]
                    extends = "a"
                    "#,
                    "global",
                )
                .expect("parse"),
            )
            .expect("add");
        assert!(matches!(
            cyclic.resolve(),
            Err(ConfigError::ProfileCycle(_))
        ));
    }

    #[test]
    fn project_cannot_define_secret_providers() {
        let project = ConfigDocument::parse(
            "[secrets.token]\nkind='environment'\nvariable='TOKEN'",
            "project",
        )
        .expect("syntactically valid");
        assert!(matches!(
            project.validate_as_project(),
            Err(ConfigError::ProjectDefinitions)
        ));
    }

    #[test]
    fn missing_secret_reference_is_an_error_and_secret_definitions_do_not_serialize() {
        let mut resolver = ConfigResolver::new();
        resolver
            .add_layer(ConfigLayer::new(
                ConfigLayerKind::Project,
                "project",
                ConfigPatch {
                    secret_refs: BTreeMap::from([("OPENAI_API_KEY".into(), "openai".into())]),
                    ..ConfigPatch::default()
                },
            ))
            .expect("layer");
        assert!(matches!(
            resolver.resolve(),
            Err(ConfigError::UnknownSecret { .. })
        ));
    }

    #[test]
    fn codex_overrides_are_flattened_before_user_arguments() {
        let config = CodexConfig {
            profile: Some("fast".into()),
            args: vec!["exec".into(), "hello world".into()],
            config: BTreeMap::from([
                ("model".into(), toml::Value::String("gpt-5".into())),
                (
                    "sandbox".into(),
                    toml::Value::Table(toml::map::Map::from_iter([(
                        "network_access".into(),
                        toml::Value::Boolean(false),
                    )])),
                ),
            ]),
        };
        assert_eq!(
            config.command_args(),
            [
                "-c",
                "model=\"gpt-5\"",
                "-c",
                "sandbox.network_access=false",
                "--profile",
                "fast",
                "exec",
                "hello world"
            ]
        );
    }

    #[test]
    fn codex_override_keys_preserve_toml_key_boundaries() {
        let config = CodexConfig {
            config: BTreeMap::from([(
                "mcp_servers".into(),
                toml::Value::Table(toml::map::Map::from_iter([
                    (
                        "docs.server".into(),
                        toml::Value::Table(toml::map::Map::from_iter([(
                            "command path".into(),
                            toml::Value::String("mcp runner".into()),
                        )])),
                    ),
                    (
                        "safe-name".into(),
                        toml::Value::Table(toml::map::Map::from_iter([(
                            "enabled".into(),
                            toml::Value::Boolean(true),
                        )])),
                    ),
                ])),
            )]),
            ..CodexConfig::default()
        };
        assert_eq!(
            config.override_args(),
            [
                "-c",
                "mcp_servers.\"docs.server\".\"command path\"=\"mcp runner\"",
                "-c",
                "mcp_servers.safe-name.enabled=true",
            ]
        );
    }

    #[test]
    fn mcp_oauth_defaults_generate_both_native_overrides() {
        let callback = CodexConfig::default()
            .mcp_oauth_callback(1_455, &[])
            .expect("callback");
        assert_eq!(callback.listener(), "127.0.0.1:1455".parse().unwrap());
        assert_eq!(callback.base_url(), "http://127.0.0.1:1455");
        assert_eq!(
            callback.generated_override_args(),
            [
                "-c",
                "mcp_oauth_callback_port=1455",
                "-c",
                "mcp_oauth_callback_url=\"http://127.0.0.1:1455\"",
            ]
        );
    }

    #[test]
    fn native_mcp_oauth_values_drive_callback_and_preserve_safe_path() {
        let config = CodexConfig {
            config: BTreeMap::from([(
                MCP_OAUTH_CALLBACK_URL.to_owned(),
                toml::Value::String("http://[::1]:8123/oauth/base/".to_owned()),
            )]),
            ..CodexConfig::default()
        };
        let callback = config.mcp_oauth_callback(1_455, &[]).expect("callback");
        assert_eq!(callback.listener(), "[::1]:8123".parse().unwrap());
        assert_eq!(callback.base_url(), "http://[::1]:8123/oauth/base/");
        assert_eq!(
            callback.generated_override_args(),
            ["-c", "mcp_oauth_callback_port=8123"]
        );
    }

    #[test]
    fn ordered_native_mcp_oauth_overrides_win_and_only_missing_key_is_generated() {
        let config = CodexConfig {
            config: BTreeMap::from([(
                MCP_OAUTH_CALLBACK_PORT.to_owned(),
                toml::Value::String("overridden invalid value".to_owned()),
            )]),
            ..CodexConfig::default()
        };
        let callback = config
            .mcp_oauth_callback(
                1_455,
                &[
                    "mcp_oauth_callback_port=9000".to_owned(),
                    "\"mcp_oauth_callback_port\"=9001".to_owned(),
                ],
            )
            .expect("callback");
        assert_eq!(callback.listener(), "127.0.0.1:9001".parse().unwrap());
        assert_eq!(
            callback.generated_override_args(),
            ["-c", "mcp_oauth_callback_url=\"http://127.0.0.1:9001\""]
        );
    }

    #[test]
    fn native_mcp_oauth_rejects_conflicts_and_unsafe_urls() {
        let config = CodexConfig {
            config: BTreeMap::from([
                (
                    MCP_OAUTH_CALLBACK_PORT.to_owned(),
                    toml::Value::Integer(8_001),
                ),
                (
                    MCP_OAUTH_CALLBACK_URL.to_owned(),
                    toml::Value::String("http://127.0.0.1:8002/base".to_owned()),
                ),
            ]),
            ..CodexConfig::default()
        };
        assert!(
            config
                .mcp_oauth_callback(1_455, &[])
                .expect_err("port mismatch")
                .to_string()
                .contains("conflicts")
        );
        for unsafe_url in [
            "https://127.0.0.1:8001",
            "http://example.test:8001",
            "http://user@127.0.0.1:8001",
            "http://127.0.0.1:8001?code=secret",
            "http://127.0.0.1:8001#fragment",
        ] {
            let config = CodexConfig {
                config: BTreeMap::from([(
                    MCP_OAUTH_CALLBACK_URL.to_owned(),
                    toml::Value::String(unsafe_url.to_owned()),
                )]),
                ..CodexConfig::default()
            };
            assert!(config.mcp_oauth_callback(1_455, &[]).is_err());
        }
    }

    proptest! {
        #[test]
        fn higher_scalar_layer_always_wins(low in any::<bool>(), high in any::<bool>()) {
            let mut resolver = ConfigResolver::new();
            resolver.add_layer(ConfigLayer::new(
                ConfigLayerKind::Global,
                "low",
                ConfigPatch { rebuild: Some(low), ..ConfigPatch::default() },
            )).expect("low");
            resolver.add_layer(ConfigLayer::new(
                ConfigLayerKind::CommandLine,
                "high",
                ConfigPatch { rebuild: Some(high), ..ConfigPatch::default() },
            )).expect("high");
            let resolved = resolver.resolve().expect("resolve");
            prop_assert_eq!(resolved.config.rebuild, high);
            prop_assert_eq!(resolved.provenance.source_for("rebuild").map(|source| source.label.as_str()), Some("high"));
        }
    }
}
