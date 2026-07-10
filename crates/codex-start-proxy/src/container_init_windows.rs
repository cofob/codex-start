//! Portable init-protocol model used by the Windows host build.
//!
//! The executable init implementation is Unix-only, but the host must be able
//! to construct and validate the JSON protocol on every supported platform.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::allowlist::Authority;

const MAX_OWNERSHIP_ROOTS: usize = 64;
const MAX_ACCOUNT_NAME_BYTES: usize = 32;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// A Unix argv value represented by a Windows platform string.
///
/// Windows callers can construct only Unicode Unix arguments. A JSON
/// `unix_base64` value is accepted when it contains UTF-8; arbitrary Unix
/// bytes remain supported by the Unix implementation of this module.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OsValue(OsString);

impl OsValue {
    #[must_use]
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

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
        self.0.to_str().ok_or_else(|| {
            serde::ser::Error::custom(
                "Windows argument contains unpaired UTF-16 and cannot be represented in a Unix execution plan",
            )
        })?.serialize(serializer)
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
            Representation::UnixBase64(encoded) => {
                let bytes = BASE64
                    .decode(encoded.unix_base64)
                    .map_err(de::Error::custom)?;
                String::from_utf8(bytes)
                    .map(|value| Self(value.into()))
                    .map_err(|_| {
                        de::Error::custom(
                            "non-UTF-8 Unix arguments cannot be represented on Windows",
                        )
                    })
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecSpec {
    pub program: OsValue,
    #[serde(default)]
    pub args: Vec<OsValue>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ExecSpec {
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

    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        std::iter::once(self.program.0.clone())
            .chain(self.args.iter().map(|argument| argument.0.clone()))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshSetup {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectServiceSpec {
    pub listen: std::net::SocketAddr,
    pub proxy: String,
    pub target: String,
    #[serde(default)]
    pub auth_token_file: Option<PathBuf>,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpProxyServiceSpec {
    pub listen: std::net::SocketAddr,
    pub proxy: String,
    pub auth_token_file: PathBuf,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    #[serde(default = "default_proxy_header_bytes")]
    pub max_header_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TcpForwardServiceSpec {
    pub listen: std::net::SocketAddr,
    pub target: String,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TcpBridgeServiceSpec {
    pub listen: std::net::SocketAddr,
    pub remote: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_auth_token_file: Option<PathBuf>,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnixBridgeServiceSpec {
    pub listen: PathBuf,
    pub remote: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_auth_token_file: Option<PathBuf>,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthTargetServiceSpec {
    pub listen: std::net::SocketAddr,
    pub callback: std::net::SocketAddr,
    pub token_file: PathBuf,
    #[serde(default = "default_service_connections")]
    pub max_connections: usize,
    #[serde(default = "default_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_handshake_timeout_seconds")]
    pub handshake_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InitServiceSpec {
    HttpProxy(HttpProxyServiceSpec),
    Connect(ConnectServiceSpec),
    TcpForward(TcpForwardServiceSpec),
    TcpBridge(TcpBridgeServiceSpec),
    UnixBridge(UnixBridgeServiceSpec),
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitSpec {
    pub version: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    #[serde(default)]
    pub account: Option<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub clear_environment: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub secret_map: Option<PathBuf>,
    #[serde(default = "default_secret_root")]
    pub secret_root: PathBuf,
    #[serde(default)]
    pub allow_insecure_secret_permissions: bool,
    #[serde(default)]
    pub ownership_paths: Vec<PathBuf>,
    pub ssh: Option<SshSetup>,
    #[serde(default)]
    pub prepare: Vec<CommandSpec>,
    #[serde(default)]
    pub services: Vec<InitServiceSpec>,
    pub command: ExecSpec,
}

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
    let mut listeners = HashSet::new();
    for service in &spec.services {
        let listener = match service {
            InitServiceSpec::HttpProxy(service) => {
                validate_http_proxy_service(service, &spec.secret_root)?;
                format!("tcp:{}", service.listen)
            }
            InitServiceSpec::Connect(service) => {
                validate_connect_service(service, &spec.secret_root)?;
                format!("tcp:{}", service.listen)
            }
            InitServiceSpec::TcpForward(service) => {
                validate_tcp_forward_service(service)?;
                format!("tcp:{}", service.listen)
            }
            InitServiceSpec::TcpBridge(service) => {
                validate_tcp_bridge_service(service, &spec.secret_root)?;
                format!("tcp:{}", service.listen)
            }
            InitServiceSpec::UnixBridge(service) => {
                validate_unix_service(service, &spec.secret_root)?;
                format!("unix:{}", posix_text(&service.listen)?)
            }
            InitServiceSpec::OauthTarget(service) => {
                validate_oauth_service(service, &spec.secret_root)?;
                format!("tcp:{}", service.listen)
            }
        };
        if !listeners.insert(listener.clone()) {
            return Err(InitError::InvalidService(format!(
                "duplicate listener {}",
                listener
                    .trim_start_matches("tcp:")
                    .trim_start_matches("unix:")
            )));
        }
    }
    validate_posix_absolute(&spec.secret_root)
        .map_err(|_| InitError::InvalidSecretRoot(spec.secret_root.clone()))?;
    Ok(())
}

fn validate_http_proxy_service(
    service: &HttpProxyServiceSpec,
    root: &Path,
) -> Result<(), InitError> {
    validate_loopback(service.listen)?;
    validate_host_port_text(&service.proxy, "proxy")?;
    validate_service_token(&service.auth_token_file, root)?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )?;
    if service.max_header_bytes < 1_024 {
        return Err(InitError::InvalidService(
            "HTTP proxy max_header_bytes must be at least 1024".into(),
        ));
    }
    Ok(())
}

fn validate_connect_service(service: &ConnectServiceSpec, root: &Path) -> Result<(), InitError> {
    validate_loopback(service.listen)?;
    validate_host_port_text(&service.proxy, "proxy")?;
    validate_host_port_text(&service.target, "target")?;
    if let Some(path) = &service.auth_token_file {
        validate_service_token(path, root)?;
    }
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )
}

fn validate_tcp_forward_service(service: &TcpForwardServiceSpec) -> Result<(), InitError> {
    validate_loopback(service.listen)?;
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
    root: &Path,
) -> Result<(), InitError> {
    validate_loopback(service.listen)?;
    validate_authenticated_bridge(
        &service.remote,
        &service.token_file,
        service.proxy.as_deref(),
        service.proxy_auth_token_file.as_deref(),
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
        root,
    )
}

fn validate_unix_service(service: &UnixBridgeServiceSpec, root: &Path) -> Result<(), InitError> {
    let listen = validate_posix_absolute(&service.listen).map_err(InitError::InvalidService)?;
    if !["/tmp/", "/var/tmp/", "/run/codex-start/", "/home/"]
        .iter()
        .any(|prefix| listen.starts_with(prefix))
    {
        return Err(InitError::InvalidService(format!(
            "unsafe Unix listener {listen}"
        )));
    }
    validate_authenticated_bridge(
        &service.remote,
        &service.token_file,
        service.proxy.as_deref(),
        service.proxy_auth_token_file.as_deref(),
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
        root,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_authenticated_bridge(
    remote: &str,
    token: &Path,
    proxy: Option<&str>,
    proxy_token: Option<&Path>,
    max: usize,
    connect: u64,
    handshake: u64,
    idle: u64,
    root: &Path,
) -> Result<(), InitError> {
    validate_host_port_text(remote, "authenticated relay")?;
    validate_service_token(token, root)?;
    if let Some(proxy) = proxy {
        validate_host_port_text(proxy, "proxy")?;
    }
    if let Some(path) = proxy_token {
        if proxy.is_none() {
            return Err(InitError::InvalidService(
                "proxy_auth_token_file requires proxy".into(),
            ));
        }
        validate_service_token(path, root)?;
    }
    validate_service_limits(max, connect, handshake, idle)
}

fn validate_oauth_service(service: &OAuthTargetServiceSpec, root: &Path) -> Result<(), InitError> {
    if service.listen.port() == 0
        || !service.callback.ip().is_loopback()
        || service.callback.port() == 0
    {
        return Err(InitError::InvalidService(
            "OAuth listeners require non-zero ports and a loopback callback".into(),
        ));
    }
    validate_service_token(&service.token_file, root)?;
    validate_service_limits(
        service.max_connections,
        service.connect_timeout_seconds,
        service.handshake_timeout_seconds,
        service.idle_timeout_seconds,
    )
}

fn validate_loopback(address: std::net::SocketAddr) -> Result<(), InitError> {
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(InitError::InvalidService(format!(
            "listener {address} must use loopback and a non-zero port"
        )));
    }
    Ok(())
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

fn validate_service_token(path: &Path, root: &Path) -> Result<(), InitError> {
    let path = validate_posix_absolute(path).map_err(InitError::InvalidService)?;
    let root = validate_posix_absolute(root).map_err(InitError::InvalidService)?;
    let under_root = path.starts_with(&format!("{}/", root.trim_end_matches('/')));
    let under_internal = path.starts_with("/run/codex-start/secrets/");
    if !under_root && !under_internal {
        return Err(InitError::InvalidService(format!(
            "service token file must be under {root} or /run/codex-start/secrets"
        )));
    }
    Ok(())
}

fn validate_service_limits(
    max: usize,
    connect: u64,
    handshake: u64,
    idle: u64,
) -> Result<(), InitError> {
    if max == 0 || connect == 0 || handshake == 0 || idle == 0 {
        return Err(InitError::InvalidService(
            "service limits and timeouts must be non-zero".into(),
        ));
    }
    Ok(())
}

fn validate_command(command: &CommandSpec) -> Result<(), InitError> {
    if command.program.is_empty() || command.program.contains('\0') {
        return Err(InitError::InvalidCommand);
    }
    for name in command.env.keys() {
        validate_environment_name(name)?;
    }
    Ok(())
}

fn validate_exec(command: &ExecSpec) -> Result<(), InitError> {
    let invalid = |value: &OsStr| {
        value.is_empty() || value.to_str().is_none() || value.to_string_lossy().contains('\0')
    };
    if invalid(command.program.as_os_str())
        || command.args.iter().any(|value| invalid(value.as_os_str()))
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
    if bytes.len() > MAX_ACCOUNT_NAME_BYTES
        || !bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        || !bytes.get(1..).is_some_and(|rest| {
            rest.iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(InitError::InvalidAccountName);
    }
    Ok(())
}

fn validate_ownership_paths(paths: &[PathBuf]) -> Result<(), InitError> {
    if paths.len() > MAX_OWNERSHIP_ROOTS {
        return Err(InitError::InvalidOwnershipPath(format!(
            "at most {MAX_OWNERSHIP_ROOTS} ownership roots are permitted"
        )));
    }
    let denied = [
        "/bin", "/dev", "/etc", "/lib", "/lib64", "/proc", "/run", "/sbin", "/sys", "/usr",
    ];
    let mut seen = HashSet::new();
    for path in paths {
        let value = validate_posix_absolute(path).map_err(InitError::InvalidOwnershipPath)?;
        if value == "/"
            || denied
                .iter()
                .any(|prefix| value == *prefix || value.starts_with(&format!("{prefix}/")))
            || !seen.insert(value.to_owned())
        {
            return Err(InitError::InvalidOwnershipPath(value.to_owned()));
        }
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), InitError> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(InitError::InvalidEnvironmentName(name.into()));
    }
    Ok(())
}

fn posix_text(path: &Path) -> Result<&str, InitError> {
    path.to_str()
        .ok_or_else(|| InitError::InvalidService("container path is not Unicode".into()))
}

fn validate_posix_absolute(path: &Path) -> Result<&str, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "container path is not Unicode".to_owned())?;
    if !value.starts_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(format!(
            "container path must be an absolute normalized POSIX path: {value}"
        ));
    }
    Ok(value)
}

pub fn direct_spec(
    options: DirectInitOptions,
    command: Vec<OsString>,
) -> Result<InitSpec, InitError> {
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
        command: ExecSpec::from_argv(command)?,
    };
    validate_spec(&spec)?;
    Ok(spec)
}

pub fn parse_prepare_json(input: &str) -> Result<CommandSpec, InitError> {
    let argv: Vec<String> = serde_json::from_str(input).map_err(InitError::ParsePrepareArgument)?;
    let mut argv = argv.into_iter();
    let command = CommandSpec {
        program: argv.next().ok_or(InitError::InvalidCommand)?,
        args: argv.collect(),
        env: BTreeMap::new(),
        cwd: None,
    };
    validate_command(&command)?;
    Ok(command)
}

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
    #[error("ownership paths require uid and gid")]
    OwnershipRequiresIdentity,
    #[error("invalid ownership path: {0}")]
    InvalidOwnershipPath(String),
    #[error("final and preparation commands require a non-empty Unicode program without NUL")]
    InvalidCommand,
    #[error("invalid environment variable name `{0}`")]
    InvalidEnvironmentName(String),
    #[error("invalid CONNECT service: {0}")]
    InvalidService(String),
    #[error("secret root must be an absolute POSIX path: {0}")]
    InvalidSecretRoot(PathBuf),
    #[error("invalid JSON argv for preparation command: {0}")]
    ParsePrepareArgument(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_posix_container_paths_on_windows() {
        assert_eq!(
            validate_posix_absolute(Path::new("/run/codex-start/token")).unwrap(),
            "/run/codex-start/token"
        );
        for invalid in [
            r"C:\\run\\token",
            r"/run\\token",
            "/run/../token",
            "relative",
        ] {
            assert!(
                validate_posix_absolute(Path::new(invalid)).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn base64_arguments_must_be_utf8_on_windows() {
        let utf8: OsValue =
            serde_json::from_value(serde_json::json!({"unix_base64": "Y29kZXg="})).unwrap();
        assert_eq!(utf8.as_os_str(), "codex");
        assert!(
            serde_json::from_value::<OsValue>(serde_json::json!({"unix_base64": "/w=="})).is_err()
        );
    }
}
