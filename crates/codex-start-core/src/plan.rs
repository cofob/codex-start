//! Validated, runtime-neutral container execution plans.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

use crate::config::{NetworkMode, ResourceLimits, RuntimeKind};
use crate::environment::PortProtocol;

/// One Unix process argument, preserved as its exact byte sequence.
///
/// Human-readable formats serialize valid UTF-8 as an ordinary string. Values
/// that are not UTF-8 use an object of the form
/// `{ "unix_base64": "..." }`. This keeps common plans readable without
/// making arbitrary Unix arguments lossy.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixArgument(OsString);

impl UnixArgument {
    /// Constructs an argument from its exact Unix byte sequence.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(OsString::from_vec(bytes))
    }

    /// Borrows the argument as a platform string.
    #[must_use]
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Borrows the exact Unix byte sequence.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Returns the argument as UTF-8 when it has a valid UTF-8 representation.
    #[must_use]
    pub fn to_str(&self) -> Option<&str> {
        self.0.to_str()
    }

    /// Consumes the wrapper and returns the exact platform string.
    #[must_use]
    pub fn into_os_string(self) -> OsString {
        self.0
    }

    fn contains_nul(&self) -> bool {
        self.as_bytes().contains(&0)
    }
}

impl AsRef<OsStr> for UnixArgument {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl From<OsString> for UnixArgument {
    fn from(value: OsString) -> Self {
        Self(value)
    }
}

impl From<&OsStr> for UnixArgument {
    fn from(value: &OsStr) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for UnixArgument {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for UnixArgument {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl Serialize for UnixArgument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(value) = self.to_str() {
            serializer.serialize_str(value)
        } else {
            BTreeMap::from([("unix_base64", BASE64.encode(self.as_bytes()))]).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for UnixArgument {
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
                .map(Self::from_bytes)
                .map_err(de::Error::custom),
        }
    }
}

/// Source backing a workload or sidecar mount.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MountSource {
    Bind { path: PathBuf },
    Volume { name: String },
    Tmpfs,
}

/// Concrete mount passed to a container runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountPlan {
    pub id: String,
    pub source: MountSource,
    pub target: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}

/// Concrete port publication. Port zero is never accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedPort {
    pub id: String,
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
    #[serde(default)]
    pub protocol: PortProtocol,
}

/// A secret is represented only by reference and destination, never by its value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretMount {
    pub name: String,
    pub provider: String,
    pub path: PathBuf,
    #[serde(default)]
    pub environment: Option<String>,
}

/// Egress proxy sidecar settings for allow-list networking.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyPlan {
    pub name: String,
    pub image: String,
    pub network_name: String,
    /// Dedicated internet-capable network used only by the egress sidecar.
    pub egress_network_name: String,
    pub listen_port: u16,
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub private_service_hosts: Vec<String>,
    /// Whether every non-health request requires per-run bearer authentication.
    #[serde(default = "default_true")]
    pub authentication_required: bool,
    #[serde(default = "default_true")]
    pub read_only: bool,
    #[serde(default = "default_cap_drop")]
    pub cap_drop: Vec<String>,
    /// Capabilities re-added after the drop set (bootstrap-only for egress).
    #[serde(default)]
    pub cap_add: Vec<String>,
}

/// Networking semantics shared by Docker and Podman adapters.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPlan {
    pub mode: NetworkMode,
    #[serde(default)]
    pub network_name: Option<String>,
    #[serde(default)]
    pub proxy: Option<ProxyPlan>,
}

impl NetworkPlan {
    #[must_use]
    pub const fn offline(network_name: String) -> Self {
        Self {
            mode: NetworkMode::Offline,
            network_name: Some(network_name),
            proxy: None,
        }
    }

    #[must_use]
    pub const fn allowlist(network_name: String, proxy: ProxyPlan) -> Self {
        Self {
            mode: NetworkMode::Allowlist,
            network_name: Some(network_name),
            proxy: Some(proxy),
        }
    }

    #[must_use]
    pub const fn bridge() -> Self {
        Self {
            mode: NetworkMode::Bridge,
            network_name: None,
            proxy: None,
        }
    }

    #[must_use]
    pub const fn host() -> Self {
        Self {
            mode: NetworkMode::Host,
            network_name: None,
            proxy: None,
        }
    }

    fn validate(&self) -> Result<(), ContainerPlanError> {
        match self.mode {
            NetworkMode::Offline => {
                require_network_name(self)?;
                if self.proxy.is_some() {
                    return Err(ContainerPlanError::InvalidNetwork(
                        "offline mode cannot have a proxy".into(),
                    ));
                }
            }
            NetworkMode::Allowlist => {
                let name = require_network_name(self)?;
                let proxy = self.proxy.as_ref().ok_or_else(|| {
                    ContainerPlanError::InvalidNetwork(
                        "allowlist mode requires an egress proxy".into(),
                    )
                })?;
                if proxy.network_name != name {
                    return Err(ContainerPlanError::InvalidNetwork(format!(
                        "proxy network `{}` does not match workload network `{name}`",
                        proxy.network_name
                    )));
                }
                validate_proxy(proxy)?;
            }
            NetworkMode::Bridge | NetworkMode::Host => {
                if self.network_name.is_some() || self.proxy.is_some() {
                    return Err(ContainerPlanError::InvalidNetwork(
                        "bridge and host modes cannot define a managed network or proxy".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn require_network_name(network: &NetworkPlan) -> Result<&str, ContainerPlanError> {
    network
        .network_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            ContainerPlanError::InvalidNetwork(
                "offline and allowlist modes require a managed network name".into(),
            )
        })
}

/// Additional container launched alongside the Codex workload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarPlan {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub entrypoint: Option<Vec<UnixArgument>>,
    #[serde(default)]
    pub command: Vec<UnixArgument>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub mounts: Vec<MountPlan>,
    #[serde(default = "default_true")]
    pub remove: bool,
    #[serde(default = "default_true")]
    pub read_only: bool,
    #[serde(default = "default_cap_drop")]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    #[serde(default)]
    pub user: Option<String>,
}

/// Complete workload description produced before a runtime is contacted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerPlan {
    pub schema_version: u32,
    pub run_id: Uuid,
    pub runtime: RuntimeKind,
    pub project_id: String,
    pub environment: String,
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub entrypoint: Option<Vec<UnixArgument>>,
    pub command: Vec<UnixArgument>,
    pub workdir: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub mounts: Vec<MountPlan>,
    #[serde(default)]
    pub ports: Vec<PublishedPort>,
    #[serde(default, skip_serializing_if = "ResourceLimits::is_empty")]
    pub resources: ResourceLimits,
    pub network: NetworkPlan,
    #[serde(default = "default_true")]
    pub tty: bool,
    #[serde(default = "default_true")]
    pub stdin: bool,
    #[serde(default = "default_true")]
    pub remove: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_cap_drop")]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub secrets: Vec<SecretMount>,
    #[serde(default)]
    pub sidecars: Vec<SidecarPlan>,
}

const fn default_true() -> bool {
    true
}

fn default_cap_drop() -> Vec<String> {
    vec!["ALL".into()]
}

impl ContainerPlan {
    pub const SCHEMA_VERSION: u32 = 1;

    /// Secure baseline plan; callers then add mounts, ports, and sidecars.
    #[must_use]
    pub fn new(
        project_id: impl Into<String>,
        environment: impl Into<String>,
        name: impl Into<String>,
        image: impl Into<String>,
        command: Vec<UnixArgument>,
        workdir: PathBuf,
        network: NetworkPlan,
    ) -> Self {
        let run_id = Uuid::new_v4();
        let project_id = project_id.into();
        Self {
            schema_version: Self::SCHEMA_VERSION,
            run_id,
            runtime: RuntimeKind::Auto,
            project_id: project_id.clone(),
            environment: environment.into(),
            name: name.into(),
            image: image.into(),
            entrypoint: None,
            command,
            workdir,
            env: BTreeMap::new(),
            labels: BTreeMap::from([
                ("io.codex-start.managed".into(), "true".into()),
                ("io.codex-start.project".into(), project_id),
                ("io.codex-start.run".into(), run_id.to_string()),
            ]),
            mounts: Vec::new(),
            ports: Vec::new(),
            resources: ResourceLimits::default(),
            network,
            tty: true,
            stdin: true,
            remove: true,
            read_only: false,
            cap_drop: default_cap_drop(),
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            user: None,
            hostname: None,
            secrets: Vec::new(),
            sidecars: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ContainerPlanError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(ContainerPlanError::UnsupportedSchema(self.schema_version));
        }
        validate_runtime_name("container", &self.name)?;
        if self.project_id.trim().is_empty() || self.environment.trim().is_empty() {
            return Err(ContainerPlanError::Invalid(
                "project_id and environment cannot be empty".into(),
            ));
        }
        if self.image.trim().is_empty() {
            return Err(ContainerPlanError::Invalid("image cannot be empty".into()));
        }
        if !self.workdir.is_absolute() {
            return Err(ContainerPlanError::Invalid(format!(
                "workdir must be absolute: {}",
                self.workdir.display()
            )));
        }
        validate_argv(&self.command, "command")?;
        if let Some(entrypoint) = &self.entrypoint {
            if entrypoint.is_empty() {
                return Err(ContainerPlanError::Invalid(
                    "entrypoint cannot be an empty array".into(),
                ));
            }
            validate_argv(entrypoint, "entrypoint")?;
        }
        validate_env(&self.env)?;
        validate_mounts(&self.mounts)?;
        validate_ports(&self.ports)?;
        self.resources
            .validate()
            .map_err(ContainerPlanError::Invalid)?;
        validate_secrets(&self.secrets)?;
        self.network.validate()?;
        if self.network.mode == NetworkMode::Host && !self.ports.is_empty() {
            return Err(ContainerPlanError::InvalidNetwork(
                "host networking cannot be combined with published ports".into(),
            ));
        }
        let mut sidecar_names = BTreeSet::new();
        for sidecar in &self.sidecars {
            validate_runtime_name("sidecar", &sidecar.name)?;
            if !sidecar_names.insert(&sidecar.name) || sidecar.name == self.name {
                return Err(ContainerPlanError::DuplicateResource(sidecar.name.clone()));
            }
            if sidecar.image.trim().is_empty() {
                return Err(ContainerPlanError::Invalid(format!(
                    "sidecar `{}` image cannot be empty",
                    sidecar.name
                )));
            }
            validate_argv(&sidecar.command, "sidecar command")?;
            if let Some(entrypoint) = &sidecar.entrypoint {
                validate_argv(entrypoint, "sidecar entrypoint")?;
            }
            validate_env(&sidecar.env)?;
            validate_mounts(&sidecar.mounts)?;
            if sidecar
                .cap_drop
                .iter()
                .chain(&sidecar.cap_add)
                .chain(&sidecar.security_opt)
                .any(|value| value.is_empty() || value.contains('\0'))
            {
                return Err(ContainerPlanError::Invalid(format!(
                    "sidecar `{}` has an invalid security option",
                    sidecar.name
                )));
            }
        }
        Ok(())
    }

    /// Redacted JSON representation. The type cannot contain resolved secret values.
    pub fn redacted_json(&self) -> Result<serde_json::Value, ContainerPlanError> {
        self.validate()?;
        serde_json::to_value(self).map_err(|error| ContainerPlanError::Serialize(error.to_string()))
    }
}

fn validate_proxy(proxy: &ProxyPlan) -> Result<(), ContainerPlanError> {
    validate_runtime_name("proxy", &proxy.name)?;
    validate_runtime_name("proxy egress network", &proxy.egress_network_name)?;
    if proxy.image.trim().is_empty() || proxy.listen_port == 0 {
        return Err(ContainerPlanError::InvalidNetwork(
            "proxy image cannot be empty and listen_port cannot be zero".into(),
        ));
    }
    if !proxy.authentication_required {
        return Err(ContainerPlanError::InvalidNetwork(
            "allowlist egress proxy authentication must be enabled".into(),
        ));
    }
    if proxy.egress_network_name == proxy.network_name {
        return Err(ContainerPlanError::InvalidNetwork(
            "egress and workload networks must be distinct".into(),
        ));
    }
    if !proxy.read_only
        || proxy.cap_drop != ["ALL"]
        || proxy.cap_add.len() != 2
        || !proxy
            .cap_add
            .iter()
            .any(|capability| capability == "SETUID")
        || !proxy
            .cap_add
            .iter()
            .any(|capability| capability == "SETGID")
    {
        return Err(ContainerPlanError::InvalidNetwork(
            "egress proxy must be read-only, drop ALL capabilities, and add only SETUID/SETGID for init bootstrap"
                .into(),
        ));
    }
    for host in proxy.allow_hosts.iter().chain(&proxy.private_service_hosts) {
        let value = host.strip_prefix("*.").unwrap_or(host);
        if !valid_proxy_rule(value) {
            return Err(ContainerPlanError::InvalidNetwork(format!(
                "invalid proxy host rule `{host}`"
            )));
        }
    }
    Ok(())
}

fn valid_proxy_rule(value: &str) -> bool {
    if value.is_empty()
        || value.contains(['/', '@'])
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    if let Some(remainder) = value.strip_prefix('[') {
        let Some((host, suffix)) = remainder.split_once(']') else {
            return false;
        };
        return host.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_proxy_port));
    }
    if value.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && !host.contains('*')
        && !host.contains([':', '[', ']'])
        && port.is_none_or(valid_proxy_port)
}

fn valid_proxy_port(value: &str) -> bool {
    value == "*" || value.parse::<u16>().is_ok_and(|port| port != 0)
}

fn validate_mounts(mounts: &[MountPlan]) -> Result<(), ContainerPlanError> {
    let mut ids = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for mount in mounts {
        validate_runtime_name("mount", &mount.id)?;
        if !ids.insert(&mount.id) {
            return Err(ContainerPlanError::DuplicateResource(mount.id.clone()));
        }
        if !mount.target.is_absolute() || mount.target == Path::new("/") {
            return Err(ContainerPlanError::Invalid(format!(
                "mount `{}` target must be an absolute non-root path",
                mount.id
            )));
        }
        if !targets.insert(&mount.target) {
            return Err(ContainerPlanError::DuplicateMountTarget(
                mount.target.clone(),
            ));
        }
        match &mount.source {
            MountSource::Bind { path } if !path.is_absolute() => {
                return Err(ContainerPlanError::Invalid(format!(
                    "bind mount `{}` source must be absolute",
                    mount.id
                )));
            }
            MountSource::Volume { name } => validate_runtime_name("volume", name)?,
            MountSource::Bind { .. } | MountSource::Tmpfs => {}
        }
    }
    Ok(())
}

fn validate_ports(ports: &[PublishedPort]) -> Result<(), ContainerPlanError> {
    let mut ids = BTreeSet::new();
    let mut bindings = BTreeSet::new();
    for port in ports {
        validate_runtime_name("port", &port.id)?;
        if port.host_port == 0 || port.container_port == 0 {
            return Err(ContainerPlanError::InvalidPort(port.id.clone()));
        }
        let address = port
            .host_ip
            .parse::<std::net::IpAddr>()
            .map_err(|_| ContainerPlanError::InvalidPort(port.id.clone()))?;
        if !ids.insert(&port.id) || !bindings.insert((address, port.host_port, port.protocol)) {
            return Err(ContainerPlanError::DuplicateResource(port.id.clone()));
        }
    }
    Ok(())
}

fn validate_secrets(secrets: &[SecretMount]) -> Result<(), ContainerPlanError> {
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for secret in secrets {
        validate_runtime_name("secret", &secret.name)?;
        if secret.provider.trim().is_empty()
            || !secret.path.is_absolute()
            || !secret.path.starts_with("/run/secrets")
            || !names.insert(&secret.name)
            || !paths.insert(&secret.path)
        {
            return Err(ContainerPlanError::InvalidSecret(secret.name.clone()));
        }
        if secret
            .environment
            .as_ref()
            .is_some_and(|name| !valid_env_name(name))
        {
            return Err(ContainerPlanError::InvalidSecret(secret.name.clone()));
        }
    }
    Ok(())
}

fn validate_argv(argv: &[UnixArgument], kind: &str) -> Result<(), ContainerPlanError> {
    if argv.iter().any(UnixArgument::contains_nul) {
        return Err(ContainerPlanError::Invalid(format!(
            "{kind} contains a NUL byte"
        )));
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), ContainerPlanError> {
    if let Some(name) = env.keys().find(|name| !valid_env_name(name)) {
        return Err(ContainerPlanError::Invalid(format!(
            "invalid environment variable name `{name}`"
        )));
    }
    if let Some(name) = env
        .iter()
        .find_map(|(name, value)| value.contains('\0').then_some(name))
    {
        return Err(ContainerPlanError::Invalid(format!(
            "environment variable `{name}` contains a NUL byte"
        )));
    }
    Ok(())
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
        })
}

fn validate_runtime_name(kind: &str, value: &str) -> Result<(), ContainerPlanError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        || value.starts_with(['-', '.'])
        || value.ends_with(['-', '.'])
    {
        return Err(ContainerPlanError::Invalid(format!(
            "invalid {kind} name `{value}`"
        )));
    }
    Ok(())
}

/// Runtime resource types tracked for cleanup and recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Container,
    Sidecar,
    Network,
    Volume,
    Worktree,
}

/// Monotonic lifecycle used by host-side RAII guards.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    #[default]
    Planned,
    Creating,
    Running,
    Stopping,
    Stopped,
    Removing,
    Removed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedResource {
    pub kind: ResourceKind,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub state: LifecycleState,
}

impl ManagedResource {
    pub fn transition(&mut self, next: LifecycleState) -> Result<(), LifecycleError> {
        use LifecycleState::{
            Creating, Failed, Planned, Removed, Removing, Running, Stopped, Stopping,
        };
        let allowed = matches!(
            (self.state, next),
            (Planned, Creating | Removing)
                | (Creating, Running | Failed | Removing)
                | (Running, Stopping | Failed)
                | (Stopping, Stopped | Failed)
                | (Stopped | Failed, Removing)
                | (Removing, Removed | Failed)
        );
        if !allowed {
            return Err(LifecycleError {
                resource: self.name.clone(),
                current: self.state,
                requested: next,
            });
        }
        self.state = next;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ContainerPlanError {
    #[error("unsupported container plan schema {0}")]
    UnsupportedSchema(u32),
    #[error("invalid container plan: {0}")]
    Invalid(String),
    #[error("invalid network plan: {0}")]
    InvalidNetwork(String),
    #[error("duplicate resource `{0}`")]
    DuplicateResource(String),
    #[error("multiple mounts target {0}")]
    DuplicateMountTarget(PathBuf),
    #[error("invalid published port `{0}`")]
    InvalidPort(String),
    #[error("invalid secret mount `{0}`")]
    InvalidSecret(String),
    #[error("failed to serialize container plan: {0}")]
    Serialize(String),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("resource `{resource}` cannot transition from {current:?} to {requested:?}")]
pub struct LifecycleError {
    pub resource: String,
    pub current: LifecycleState,
    pub requested: LifecycleState,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn proxy(network: &str) -> ProxyPlan {
        ProxyPlan {
            name: "proxy-one".into(),
            image: "ghcr.io/example/proxy@sha256:123".into(),
            network_name: network.into(),
            egress_network_name: "proxy-egress-one".into(),
            listen_port: 3128,
            allow_hosts: vec!["api.openai.com".into(), "*.github.com".into()],
            private_service_hosts: Vec::new(),
            authentication_required: true,
            read_only: true,
            cap_drop: vec!["ALL".into()],
            cap_add: vec!["SETUID".into(), "SETGID".into()],
        }
    }

    fn valid_plan() -> ContainerPlan {
        let network = "codex-net-one".to_owned();
        let mut plan = ContainerPlan::new(
            "project123",
            "rust",
            "codex-project-run",
            "ghcr.io/example/rust@sha256:123",
            vec!["codex".into(), "exec".into()],
            PathBuf::from("/workspaces/project123/main"),
            NetworkPlan::allowlist(network.clone(), proxy(&network)),
        );
        plan.mounts.push(MountPlan {
            id: "workspace".into(),
            source: MountSource::Bind {
                path: PathBuf::from("/host/project"),
            },
            target: PathBuf::from("/workspaces/project123/main"),
            read_only: false,
        });
        plan.ports.push(PublishedPort {
            id: "dev".into(),
            host_ip: "127.0.0.1".into(),
            host_port: 5173,
            container_port: 5173,
            protocol: PortProtocol::Tcp,
        });
        plan.secrets.push(SecretMount {
            name: "openai".into(),
            provider: "openai-key".into(),
            path: PathBuf::from("/run/secrets/openai"),
            environment: Some("OPENAI_API_KEY".into()),
        });
        plan
    }

    #[test]
    fn complete_plan_validates_and_json_has_no_secret_value() {
        let plan = valid_plan();
        plan.validate().expect("valid");
        let json = plan.redacted_json().expect("json").to_string();
        assert!(json.contains("openai-key"));
        assert!(!json.contains("secret_value"));
    }

    #[test]
    fn utf8_argument_serializes_as_a_readable_string() {
        let argument = UnixArgument::from("codex exec --json");
        let value = serde_json::to_value(&argument).expect("serialize argument");
        assert_eq!(value, serde_json::json!("codex exec --json"));
        assert_eq!(
            serde_json::from_value::<UnixArgument>(value).expect("deserialize argument"),
            argument
        );
    }

    #[test]
    fn non_utf8_argv_roundtrips_through_the_complete_plan() {
        let non_utf8 = UnixArgument::from_bytes(vec![b'f', 0x80, b'o']);
        assert_eq!(
            serde_json::to_value(&non_utf8).expect("serialize argument"),
            serde_json::json!({"unix_base64": "ZoBv"})
        );

        let mut plan = valid_plan();
        plan.entrypoint = Some(vec![UnixArgument::from("/usr/bin/env"), non_utf8.clone()]);
        plan.command.push(non_utf8.clone());
        plan.sidecars.push(SidecarPlan {
            name: "helper".into(),
            image: "example.invalid/helper:latest".into(),
            entrypoint: Some(vec![non_utf8.clone()]),
            command: vec![non_utf8],
            env: BTreeMap::new(),
            mounts: Vec::new(),
            remove: true,
            read_only: true,
            cap_drop: vec!["ALL".into()],
            cap_add: Vec::new(),
            security_opt: Vec::new(),
            user: None,
        });
        plan.validate().expect("non-UTF-8 without NUL is valid");

        let encoded = serde_json::to_vec(&plan).expect("serialize plan");
        let decoded: ContainerPlan = serde_json::from_slice(&encoded).expect("deserialize plan");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn validation_rejects_only_an_actual_nul_byte_in_lossless_argv() {
        let mut plan = valid_plan();
        plan.command = vec![UnixArgument::from_bytes(vec![0xC0, 0x80])];
        plan.validate()
            .expect("a non-UTF-8 sequence resembling an overlong NUL is valid argv");

        plan.command = vec![UnixArgument::from_bytes(vec![0x80, 0, 0x81])];
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::Invalid(message)) if message == "command contains a NUL byte"
        ));
    }

    #[test]
    fn encoded_argument_requires_valid_base64_and_no_extra_fields() {
        assert!(
            serde_json::from_value::<UnixArgument>(serde_json::json!({
                "unix_base64": "not base64"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<UnixArgument>(serde_json::json!({
                "unix_base64": "YQ==",
                "unexpected": true
            }))
            .is_err()
        );
    }

    #[test]
    fn catches_duplicate_mount_targets_and_host_port_conflict() {
        let mut plan = valid_plan();
        plan.mounts.push(MountPlan {
            id: "other".into(),
            source: MountSource::Tmpfs,
            target: plan.mounts[0].target.clone(),
            read_only: false,
        });
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::DuplicateMountTarget(_))
        ));

        let mut plan = valid_plan();
        plan.network = NetworkPlan::host();
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::InvalidNetwork(_))
        ));
    }

    #[test]
    fn allowlist_requires_matching_proxy() {
        let mut plan = valid_plan();
        plan.network.proxy.as_mut().expect("proxy").network_name = "wrong".into();
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::InvalidNetwork(_))
        ));
    }

    #[test]
    fn lifecycle_allows_cleanup_after_failure_but_rejects_resurrection() {
        let mut resource = ManagedResource {
            kind: ResourceKind::Container,
            name: "run".into(),
            labels: BTreeMap::new(),
            state: LifecycleState::Planned,
        };
        resource
            .transition(LifecycleState::Creating)
            .expect("creating");
        resource.transition(LifecycleState::Failed).expect("failed");
        resource
            .transition(LifecycleState::Removing)
            .expect("removing");
        resource
            .transition(LifecycleState::Removed)
            .expect("removed");
        assert!(resource.transition(LifecycleState::Running).is_err());
    }

    #[test]
    fn validates_secret_destination_and_bind_source() {
        let mut plan = valid_plan();
        plan.secrets[0].path = PathBuf::from("/tmp/leak");
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::InvalidSecret(_))
        ));
        let mut plan = valid_plan();
        plan.mounts[0].source = MountSource::Bind {
            path: PathBuf::from("relative"),
        };
        assert!(matches!(
            plan.validate(),
            Err(ContainerPlanError::Invalid(_))
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_unix_arguments_roundtrip_through_json(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let argument = UnixArgument::from_bytes(bytes.clone());
            let encoded = serde_json::to_vec(&argument).expect("serialize arbitrary argument");
            let decoded: UnixArgument =
                serde_json::from_slice(&encoded).expect("deserialize arbitrary argument");
            prop_assert_eq!(decoded.as_bytes(), bytes.as_slice());
        }

        #[test]
        fn nul_detection_uses_exact_argument_bytes(
            prefix in prop::collection::vec(1_u8..=u8::MAX, 0..128),
            suffix in prop::collection::vec(1_u8..=u8::MAX, 0..128),
        ) {
            let mut bytes = prefix;
            bytes.push(0);
            bytes.extend(suffix);
            let mut plan = valid_plan();
            plan.command = vec![UnixArgument::from_bytes(bytes)];
            prop_assert!(matches!(plan.validate(), Err(ContainerPlanError::Invalid(_))));
        }
    }
}
