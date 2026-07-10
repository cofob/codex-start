//! Declarative development environment definitions and inheritance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{ConfigPatch, valid_dns_or_ip, valid_host_rule_syntax};

/// Current on-disk environment manifest schema.
pub const ENVIRONMENT_SCHEMA_VERSION: u32 = 1;

/// Directory below the application configuration directory containing manifests.
pub const ENVIRONMENTS_DIRECTORY: &str = "environments";

/// Where a manifest came from, used in diagnostics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ManifestSource {
    /// Human-readable source (normally a path or `built-in:<name>`).
    pub display: String,
    /// Whether this definition is shipped with the application.
    #[serde(default)]
    pub built_in: bool,
}

impl ManifestSource {
    #[must_use]
    pub fn built_in(name: &str) -> Self {
        Self {
            display: format!("built-in:{name}"),
            built_in: true,
        }
    }

    #[must_use]
    pub fn file(path: &Path) -> Self {
        Self {
            display: path.display().to_string(),
            built_in: false,
        }
    }
}

/// An argv-based command executed inside the workload container.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn validate(&self) -> Result<(), EnvironmentError> {
        if self.program.trim().is_empty() {
            return Err(EnvironmentError::Invalid(
                "prepare command program cannot be empty".into(),
            ));
        }
        if self.program.contains('\0') || self.args.iter().any(|arg| arg.contains('\0')) {
            return Err(EnvironmentError::Invalid(
                "prepare command cannot contain a NUL byte".into(),
            ));
        }
        validate_prepare_secrets(self)?;
        Ok(())
    }
}

/// Reproducible custom image build settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    pub context: PathBuf,
    pub dockerfile: PathBuf,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Mount implementation.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    #[default]
    Bind,
    Volume,
    Tmpfs,
}

/// A stable-ID mount definition. `remove` deletes an inherited resource.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    pub id: String,
    #[serde(default)]
    pub source: Option<PathBuf>,
    #[serde(default)]
    pub target: PathBuf,
    #[serde(default)]
    pub kind: MountKind,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub remove: bool,
}

/// Lifetime/naming scope for a persistent cache volume.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    Shared,
    #[default]
    Project,
    Environment,
    Run,
}

/// A named persistent cache mounted into the container.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSpec {
    pub id: String,
    #[serde(default)]
    pub target: PathBuf,
    #[serde(default)]
    pub scope: CacheScope,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub remove: bool,
}

/// Transport protocol for a published port.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

/// A loopback-safe port publishing definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSpec {
    pub id: String,
    #[serde(default)]
    pub container: u16,
    #[serde(default)]
    pub host: Option<u16>,
    #[serde(default = "default_host_ip")]
    pub host_ip: String,
    #[serde(default)]
    pub protocol: PortProtocol,
    #[serde(default)]
    pub remove: bool,
}

fn default_host_ip() -> String {
    "127.0.0.1".into()
}

/// A host endpoint intentionally made available to a workload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostServiceSpec {
    pub id: String,
    #[serde(default = "default_host_gateway")]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub container_host: Option<String>,
    #[serde(default)]
    pub container_port: Option<u16>,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default)]
    pub remove: bool,
}

fn default_host_gateway() -> String {
    "host.containers.internal".into()
}

/// A schema-v1 environment manifest. Every field except `name` is inheritable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentManifest {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    /// Launcher defaults contributed below global/profile/project settings.
    #[serde(default)]
    pub settings: ConfigPatch,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub build: Option<BuildSpec>,
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markers: Option<Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<Vec<CommandSpec>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Environment variables populated from globally trusted secret providers.
    #[serde(default)]
    pub secret_refs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_hosts: Option<Vec<String>>,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    #[serde(default)]
    pub caches: Vec<CacheSpec>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub host_services: Vec<HostServiceSpec>,
}

const fn schema_version() -> u32 {
    ENVIRONMENT_SCHEMA_VERSION
}

impl EnvironmentManifest {
    pub fn parse(input: &str, source: impl Into<String>) -> Result<Self, EnvironmentError> {
        let source = source.into();
        let manifest: Self = toml::from_str(input).map_err(|error| {
            let message = error.to_string();
            EnvironmentError::Parse {
                source_label: source.clone(),
                suggestion: environment_unknown_field_suggestion(&message),
                message,
                span: error.span(),
            }
        })?;
        manifest.validate().map_err(|error| match error {
            EnvironmentError::Invalid(message) => EnvironmentError::InvalidManifest {
                name: manifest.name.clone(),
                source_label: source,
                message,
            },
            other => other,
        })?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), EnvironmentError> {
        validate_manifest_header(self)?;
        validate_manifest_resources(self)?;
        for host in self.allow_hosts.iter().flatten() {
            validate_host_rule(host)?;
        }
        Ok(())
    }
}

fn validate_manifest_header(manifest: &EnvironmentManifest) -> Result<(), EnvironmentError> {
    if manifest.schema_version != ENVIRONMENT_SCHEMA_VERSION {
        return Err(EnvironmentError::UnsupportedSchema {
            found: manifest.schema_version,
            supported: ENVIRONMENT_SCHEMA_VERSION,
        });
    }
    validate_identifier("environment", &manifest.name)?;
    if manifest.extends.as_deref() == Some(manifest.name.as_str()) {
        return Err(EnvironmentError::Invalid(
            "an environment cannot extend itself".into(),
        ));
    }
    manifest
        .settings
        .validate()
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
    if manifest.settings.profile.is_some() || manifest.settings.environment.is_some() {
        return Err(EnvironmentError::Invalid(
            "environment defaults cannot select a profile or another environment".into(),
        ));
    }
    if manifest.image.is_some() && manifest.build.is_some() {
        return Err(EnvironmentError::Invalid(
            "image and build are mutually exclusive".into(),
        ));
    }
    if let Some(image) = &manifest.image {
        validate_oci_reference(image)?;
    }
    if manifest
        .workdir
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(EnvironmentError::Invalid(
            "container workdir must be absolute".into(),
        ));
    }
    for command in manifest.prepare.iter().flatten() {
        command.validate()?;
    }
    if let Some(build) = &manifest.build {
        validate_build_args(&build.args)?;
    }
    validate_env(&manifest.env)?;
    validate_secret_refs(&manifest.secret_refs)?;
    Ok(())
}

fn validate_manifest_resources(manifest: &EnvironmentManifest) -> Result<(), EnvironmentError> {
    validate_resource_ids(&manifest.mounts, |item| &item.id, "mount")?;
    validate_resource_ids(&manifest.caches, |item| &item.id, "cache")?;
    validate_resource_ids(&manifest.ports, |item| &item.id, "port")?;
    validate_resource_ids(&manifest.host_services, |item| &item.id, "host service")?;
    for mount in &manifest.mounts {
        if mount.remove {
            continue;
        }
        validate_absolute_target("mount", &mount.target)?;
        match (mount.kind, mount.source.as_ref()) {
            (MountKind::Bind | MountKind::Volume, None) => {
                return Err(EnvironmentError::Invalid(format!(
                    "mount `{}` requires a source",
                    mount.id
                )));
            }
            (MountKind::Tmpfs, Some(_)) => {
                return Err(EnvironmentError::Invalid(format!(
                    "tmpfs mount `{}` cannot have a source",
                    mount.id
                )));
            }
            _ => {}
        }
    }
    for cache in &manifest.caches {
        if cache.remove {
            continue;
        }
        validate_absolute_target("cache", &cache.target)?;
    }
    for port in &manifest.ports {
        if !port.remove && (port.container == 0 || port.host == Some(0)) {
            return Err(EnvironmentError::Invalid(format!(
                "port `{}` must use values in 1..=65535",
                port.id
            )));
        }
        if !port.remove && port.host_ip.parse::<std::net::IpAddr>().is_err() {
            return Err(EnvironmentError::Invalid(format!(
                "port `{}` has invalid host_ip `{}`",
                port.id, port.host_ip
            )));
        }
    }
    for service in &manifest.host_services {
        if service.remove {
            continue;
        }
        if service.port == 0 || service.container_port == Some(0) {
            return Err(EnvironmentError::Invalid(format!(
                "host service `{}` has an invalid zero port",
                service.id
            )));
        }
        if !valid_host_service_target(&service.host) {
            return Err(EnvironmentError::Invalid(format!(
                "host service `{}` has invalid target host `{}`",
                service.id, service.host
            )));
        }
        if service
            .container_host
            .as_deref()
            .is_some_and(|alias| !valid_container_alias(alias))
        {
            return Err(EnvironmentError::Invalid(format!(
                "host service `{}` has invalid container_host alias",
                service.id
            )));
        }
    }
    Ok(())
}

fn valid_host_service_target(host: &str) -> bool {
    !host.contains('*') && valid_dns_or_ip(host)
}

fn valid_container_alias(alias: &str) -> bool {
    if !alias.is_ascii() {
        return false;
    }
    match alias.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Ok(std::net::IpAddr::V6(_)) => false,
        Err(_) => !alias.contains('*') && valid_dns_or_ip(alias),
    }
}

/// A fully inherited and validated environment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedEnvironment {
    pub name: String,
    pub description: Option<String>,
    pub ancestry: Vec<String>,
    pub settings: ConfigPatch,
    pub image: Option<String>,
    pub build: Option<BuildSpec>,
    pub workdir: PathBuf,
    pub user: Option<String>,
    pub markers: Vec<PathBuf>,
    pub prepare: Vec<CommandSpec>,
    pub env: BTreeMap<String, String>,
    pub secret_refs: BTreeMap<String, String>,
    pub allow_hosts: Vec<String>,
    pub mounts: Vec<MountSpec>,
    pub caches: Vec<CacheSpec>,
    pub ports: Vec<PortSpec>,
    pub host_services: Vec<HostServiceSpec>,
}

impl ResolvedEnvironment {
    #[must_use]
    pub fn matches_project(&self, root: &Path) -> bool {
        self.markers.iter().all(|marker| root.join(marker).exists())
    }

    pub fn validate_project(&self, root: &Path) -> Result<(), EnvironmentError> {
        let missing = self
            .markers
            .iter()
            .filter(|marker| !root.join(marker).exists())
            .map(|marker| marker.display().to_string())
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(EnvironmentError::MissingMarkers {
                environment: self.name.clone(),
                root: root.to_path_buf(),
                markers: missing,
            })
        }
    }
}

#[derive(Clone, Debug)]
struct RegisteredManifest {
    manifest: EnvironmentManifest,
    source: ManifestSource,
}

/// Collection of named manifests with deterministic inheritance resolution.
#[derive(Clone, Debug, Default)]
pub struct EnvironmentRegistry {
    manifests: BTreeMap<String, RegisteredManifest>,
}

impl EnvironmentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        manifest: EnvironmentManifest,
        source: ManifestSource,
    ) -> Result<(), EnvironmentError> {
        manifest
            .validate()
            .map_err(|error| EnvironmentError::InvalidManifest {
                name: manifest.name.clone(),
                source_label: source.display.clone(),
                message: error.to_string(),
            })?;
        if let Some(existing) = self.manifests.get(&manifest.name) {
            return Err(EnvironmentError::Duplicate {
                name: manifest.name,
                first: existing.source.display.clone(),
                second: source.display,
            });
        }
        self.manifests.insert(
            manifest.name.clone(),
            RegisteredManifest { manifest, source },
        );
        Ok(())
    }

    pub fn replace(
        &mut self,
        manifest: EnvironmentManifest,
        source: ManifestSource,
    ) -> Result<Option<ManifestSource>, EnvironmentError> {
        manifest.validate()?;
        Ok(self
            .manifests
            .insert(
                manifest.name.clone(),
                RegisteredManifest { manifest, source },
            )
            .map(|previous| previous.source))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.manifests.keys().map(String::as_str)
    }

    /// Return one validated, unresolved manifest.
    #[must_use]
    pub fn manifest(&self, name: &str) -> Option<&EnvironmentManifest> {
        self.manifests.get(name).map(|entry| &entry.manifest)
    }

    #[must_use]
    pub fn source(&self, name: &str) -> Option<&ManifestSource> {
        self.manifests.get(name).map(|entry| &entry.source)
    }

    pub fn resolve(&self, name: &str) -> Result<ResolvedEnvironment, EnvironmentError> {
        let ancestry = self.ancestry(name)?;
        let mut visiting = Vec::new();
        let manifest = self.resolve_manifest(name, &mut visiting)?;
        let resolved = ResolvedEnvironment {
            name: manifest.name,
            description: manifest.description,
            ancestry,
            settings: manifest.settings,
            image: manifest.image,
            build: manifest.build,
            workdir: manifest
                .workdir
                .unwrap_or_else(|| PathBuf::from("/workspace")),
            user: manifest.user,
            markers: manifest.markers.unwrap_or_default(),
            prepare: manifest.prepare.unwrap_or_default(),
            env: manifest.env,
            secret_refs: manifest.secret_refs,
            allow_hosts: deduplicate(manifest.allow_hosts.unwrap_or_default()),
            mounts: without_removed(manifest.mounts),
            caches: without_removed(manifest.caches),
            ports: without_removed(manifest.ports),
            host_services: without_removed(manifest.host_services),
        };
        if resolved.image.is_none() && resolved.build.is_none() {
            return Err(EnvironmentError::Invalid(format!(
                "resolved environment `{name}` has neither image nor build"
            )));
        }
        Ok(resolved)
    }

    fn ancestry(&self, name: &str) -> Result<Vec<String>, EnvironmentError> {
        let mut lineage = Vec::new();
        let mut current = name;
        loop {
            if let Some(position) = lineage.iter().position(|item| item == current) {
                let mut cycle = lineage[position..].to_vec();
                cycle.push(current.to_owned());
                return Err(EnvironmentError::InheritanceCycle(cycle.join(" -> ")));
            }
            lineage.push(current.to_owned());
            let entry = self
                .manifests
                .get(current)
                .ok_or_else(|| EnvironmentError::Unknown(current.to_owned()))?;
            if let Some(parent) = &entry.manifest.extends {
                current = parent;
            } else {
                lineage.reverse();
                return Ok(lineage);
            }
        }
    }

    fn resolve_manifest(
        &self,
        name: &str,
        stack: &mut Vec<String>,
    ) -> Result<EnvironmentManifest, EnvironmentError> {
        if let Some(position) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[position..].to_vec();
            cycle.push(name.to_owned());
            return Err(EnvironmentError::InheritanceCycle(cycle.join(" -> ")));
        }
        let entry = self
            .manifests
            .get(name)
            .ok_or_else(|| EnvironmentError::Unknown(name.to_owned()))?;
        stack.push(name.to_owned());
        let result = if let Some(parent) = &entry.manifest.extends {
            let parent = self.resolve_manifest(parent, stack)?;
            merge_manifest(parent, entry.manifest.clone())?
        } else {
            entry.manifest.clone()
        };
        stack.pop();
        Ok(result)
    }

    /// Select the most specific matching environment. Ties are lexical for reproducibility.
    pub fn detect(&self, root: &Path) -> Result<Option<String>, EnvironmentError> {
        let mut candidates = self
            .names()
            .map(|name| self.resolve(name))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|environment| {
                !environment.markers.is_empty() && environment.matches_project(root)
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .markers
                .len()
                .cmp(&left.markers.len())
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(candidates
            .first()
            .map(|environment| environment.name.clone()))
    }
}

fn merge_manifest(
    mut parent: EnvironmentManifest,
    child: EnvironmentManifest,
) -> Result<EnvironmentManifest, EnvironmentError> {
    parent.schema_version = child.schema_version;
    parent.name = child.name;
    parent.extends = None;
    parent.settings = parent
        .settings
        .merged_with(&child.settings)
        .map_err(|error| EnvironmentError::Invalid(error.to_string()))?;
    if child.description.is_some() {
        parent.description = child.description;
    }
    if child.image.is_some() || child.build.is_some() {
        parent.image = child.image;
        parent.build = child.build;
    }
    if child.workdir.is_some() {
        parent.workdir = child.workdir;
    }
    if child.user.is_some() {
        parent.user = child.user;
    }
    if child.markers.is_some() {
        parent.markers = child.markers;
    }
    if child.prepare.is_some() {
        parent.prepare = child.prepare;
    }
    parent.env.extend(child.env);
    parent.secret_refs.extend(child.secret_refs);
    if child.allow_hosts.is_some() {
        parent.allow_hosts = child.allow_hosts.map(deduplicate);
    }
    parent.mounts = merge_by_id(
        parent.mounts,
        child.mounts,
        |item| &item.id,
        |item| item.remove,
    );
    parent.caches = merge_by_id(
        parent.caches,
        child.caches,
        |item| &item.id,
        |item| item.remove,
    );
    parent.ports = merge_by_id(
        parent.ports,
        child.ports,
        |item| &item.id,
        |item| item.remove,
    );
    parent.host_services = merge_by_id(
        parent.host_services,
        child.host_services,
        |item| &item.id,
        |item| item.remove,
    );
    Ok(parent)
}

fn validate_oci_reference(image: &str) -> Result<(), EnvironmentError> {
    let image = image.trim();
    if image.is_empty() || image.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(EnvironmentError::Invalid(
            "image must be a non-empty OCI reference without whitespace".into(),
        ));
    }
    if let Some((_, digest)) = image.rsplit_once('@') {
        let Some(hex) = digest.strip_prefix("sha256:") else {
            return Err(EnvironmentError::Invalid(
                "image digests must use sha256".into(),
            ));
        };
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EnvironmentError::Invalid(
                "image sha256 digest must contain exactly 64 hexadecimal digits".into(),
            ));
        }
        return Ok(());
    }
    let leaf = image.rsplit('/').next().unwrap_or(image);
    let Some((_, tag)) = leaf.rsplit_once(':') else {
        return Err(EnvironmentError::Invalid(
            "image references must use an explicit non-latest tag or sha256 digest".into(),
        ));
    };
    if tag.is_empty() || tag.eq_ignore_ascii_case("latest") {
        return Err(EnvironmentError::Invalid(
            "image references cannot use an empty or latest tag".into(),
        ));
    }
    Ok(())
}

fn merge_by_id<T: Clone>(
    parent: Vec<T>,
    child: Vec<T>,
    id: impl Fn(&T) -> &str,
    removed: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut items = parent
        .into_iter()
        .map(|item| (id(&item).to_owned(), item))
        .collect::<BTreeMap<_, _>>();
    for item in child {
        if removed(&item) {
            items.remove(id(&item));
        } else {
            items.insert(id(&item).to_owned(), item);
        }
    }
    items.into_values().collect()
}

trait Removable {
    fn removed(&self) -> bool;
}

macro_rules! removable {
    ($($type:ty),+ $(,)?) => { $(
        impl Removable for $type {
            fn removed(&self) -> bool { self.remove }
        }
    )+ };
}

removable!(MountSpec, CacheSpec, PortSpec, HostServiceSpec);

fn without_removed<T: Removable>(items: Vec<T>) -> Vec<T> {
    items.into_iter().filter(|item| !item.removed()).collect()
}

fn deduplicate(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), EnvironmentError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(EnvironmentError::Invalid(format!(
            "{kind} identifier `{value}` must contain 1-64 lowercase ASCII letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

fn validate_resource_ids<T>(
    resources: &[T],
    id: impl Fn(&T) -> &str,
    kind: &str,
) -> Result<(), EnvironmentError> {
    let mut ids = BTreeSet::new();
    for resource in resources {
        let value = id(resource);
        validate_identifier(kind, value)?;
        if !ids.insert(value) {
            return Err(EnvironmentError::Invalid(format!(
                "duplicate {kind} id `{value}`"
            )));
        }
    }
    Ok(())
}

fn validate_absolute_target(kind: &str, path: &Path) -> Result<(), EnvironmentError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(EnvironmentError::Invalid(format!(
            "{kind} target `{}` must be an absolute non-root path",
            path.display()
        )));
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), EnvironmentError> {
    for key in env.keys() {
        if !valid_environment_name(key) {
            return Err(EnvironmentError::Invalid(format!(
                "invalid environment variable name `{key}`"
            )));
        }
        if sensitive_environment_name(key) {
            return Err(EnvironmentError::Invalid(format!(
                "environment variable `{key}` looks secret-bearing; use [secret_refs] with a global provider"
            )));
        }
    }
    Ok(())
}

fn validate_secret_refs(secret_refs: &BTreeMap<String, String>) -> Result<(), EnvironmentError> {
    for (environment, provider) in secret_refs {
        if !valid_environment_name(environment) {
            return Err(EnvironmentError::Invalid(format!(
                "invalid secret target environment variable `{environment}`"
            )));
        }
        validate_identifier("secret provider", provider)?;
    }
    Ok(())
}

fn validate_build_args(args: &BTreeMap<String, String>) -> Result<(), EnvironmentError> {
    for (key, value) in args {
        if ((sensitive_setting_name(key) || secret_environment_name_setting(key))
            && !safe_secret_reference(key, value))
            || sensitive_assignment(value)
                .is_some_and(|(name, value)| !safe_secret_reference(name, value))
        {
            return Err(EnvironmentError::Invalid(format!(
                "build argument `{key}` looks like a plaintext secret; use an environment-variable reference"
            )));
        }
    }
    Ok(())
}

fn validate_prepare_secrets(command: &CommandSpec) -> Result<(), EnvironmentError> {
    for argument in &command.args {
        if let Some((name, value)) = sensitive_assignment(argument) {
            if !safe_secret_reference(name, value) {
                return Err(plaintext_prepare_secret(name));
            }
            continue;
        }
        validate_sensitive_pairs(argument.split_ascii_whitespace())?;
    }
    validate_sensitive_pairs(command.args.iter().map(String::as_str))
}

fn validate_sensitive_pairs<'a>(
    arguments: impl IntoIterator<Item = &'a str>,
) -> Result<(), EnvironmentError> {
    let mut arguments = arguments.into_iter().peekable();
    while let Some(argument) = arguments.next() {
        if secret_environment_name_setting(argument) {
            let Some(value) = arguments.peek() else {
                continue;
            };
            if !valid_environment_name(trim_shell_punctuation(value)) {
                return Err(plaintext_prepare_secret(argument));
            }
            continue;
        }
        if environment_name_setting(argument) {
            continue;
        }
        if sensitive_setting_name(argument) {
            let Some(value) = arguments.peek() else {
                continue;
            };
            if !environment_reference(value) {
                return Err(plaintext_prepare_secret(argument));
            }
        }
    }
    Ok(())
}

fn plaintext_prepare_secret(name: &str) -> EnvironmentError {
    EnvironmentError::Invalid(format!(
        "prepare argument `{name}` looks like a plaintext secret; read the injected environment variable instead"
    ))
}

fn sensitive_assignment(value: &str) -> Option<(&str, &str)> {
    ['=', ':'].into_iter().find_map(|separator| {
        let (name, value) = value.split_once(separator)?;
        (sensitive_setting_name(name) || secret_environment_name_setting(name))
            .then_some((name, value))
    })
}

fn safe_secret_reference(name: &str, value: &str) -> bool {
    if environment_name_setting(name) {
        return valid_environment_name(trim_shell_punctuation(value));
    }
    environment_reference(value) || authorization_environment_reference(value)
}

fn environment_reference(value: &str) -> bool {
    let value = trim_shell_punctuation(value);
    let Some(reference) = value.strip_prefix('$') else {
        return false;
    };
    let reference = reference
        .strip_prefix('{')
        .and_then(|reference| reference.strip_suffix('}'))
        .unwrap_or(reference);
    valid_environment_name(reference)
}

fn authorization_environment_reference(value: &str) -> bool {
    let value = trim_shell_punctuation(value);
    let Some((scheme, reference)) = value.split_once(char::is_whitespace) else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "basic" | "bearer" | "token"
    ) && environment_reference(reference)
}

fn trim_shell_punctuation(value: &str) -> &str {
    value.trim().trim_matches(['\'', '"'])
}

fn environment_name_setting(name: &str) -> bool {
    let name = normalized_setting_name(name);
    name == "env"
        || name == "env_key"
        || name.ends_with("_env")
        || name.contains("env_var")
        || name.contains("environment_variable")
}

fn sensitive_setting_name(name: &str) -> bool {
    !environment_name_setting(name) && raw_sensitive_setting_name(name)
}

fn secret_environment_name_setting(name: &str) -> bool {
    environment_name_setting(name) && raw_sensitive_setting_name(name)
}

fn raw_sensitive_setting_name(name: &str) -> bool {
    let name = normalized_setting_name(name);
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
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| contains_name_phrase(&name, marker))
}

fn contains_name_phrase(name: &str, phrase: &str) -> bool {
    name == phrase
        || name.starts_with(&format!("{phrase}_"))
        || name.ends_with(&format!("_{phrase}"))
        || name.contains(&format!("_{phrase}_"))
}

fn normalized_setting_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator = false;
    for character in name.trim().trim_start_matches('-').chars() {
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

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphanumeric() && (index > 0 || !byte.is_ascii_digit())
        })
}

fn sensitive_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "API_KEY",
        "ACCESS_KEY",
        "AUTH_TOKEN",
        "CLIENT_SECRET",
        "CREDENTIAL",
        "PASSWORD",
        "PRIVATE_KEY",
        "SECRET",
        "TOKEN",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

fn validate_host_rule(rule: &str) -> Result<(), EnvironmentError> {
    if !valid_host_rule_syntax(rule) {
        return Err(EnvironmentError::Invalid(format!(
            "invalid allow-list host rule `{rule}`"
        )));
    }
    Ok(())
}

fn environment_unknown_field_suggestion(message: &str) -> Option<String> {
    const FIELDS: &[&str] = &[
        "schema_version",
        "name",
        "description",
        "extends",
        "settings",
        "image",
        "build",
        "workdir",
        "user",
        "markers",
        "prepare",
        "env",
        "secret_refs",
        "allow_hosts",
        "mounts",
        "caches",
        "ports",
        "host_services",
        "context",
        "dockerfile",
        "target",
        "args",
        "program",
        "id",
        "source",
        "kind",
        "read_only",
        "remove",
        "scope",
        "container",
        "host",
        "host_ip",
        "protocol",
        "port",
        "container_host",
        "container_port",
        "allow_private",
    ];
    let marker = "unknown field `";
    let start = message.find(marker)? + marker.len();
    let end = message[start..].find('`')? + start;
    let unknown = &message[start..end];
    FIELDS
        .iter()
        .map(|candidate| (*candidate, environment_edit_distance(unknown, candidate)))
        .min_by_key(|(_, distance)| *distance)
        .filter(|(_, distance)| *distance <= 3)
        .map(|(candidate, _)| candidate.to_owned())
}

fn environment_edit_distance(left: &str, right: &str) -> usize {
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
pub enum EnvironmentError {
    #[error("failed to parse environment manifest {source_label}: {message}{suggestion_text}", suggestion_text = suggestion.as_ref().map(|value| format!("; did you mean `{value}`?")).unwrap_or_default())]
    Parse {
        source_label: String,
        message: String,
        span: Option<std::ops::Range<usize>>,
        suggestion: Option<String>,
    },
    #[error("unsupported environment schema {found}; this version supports {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("invalid environment manifest `{name}` from {source_label}: {message}")]
    InvalidManifest {
        name: String,
        source_label: String,
        message: String,
    },
    #[error("invalid environment: {0}")]
    Invalid(String),
    #[error("environment `{0}` is not defined")]
    Unknown(String),
    #[error("environment `{name}` is defined twice ({first} and {second})")]
    Duplicate {
        name: String,
        first: String,
        second: String,
    },
    #[error("environment inheritance cycle: {0}")]
    InheritanceCycle(String),
    #[error("environment `{environment}` requires markers {markers:?} below {root}")]
    MissingMarkers {
        environment: String,
        root: PathBuf,
        markers: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn manifest(input: &str) -> EnvironmentManifest {
        EnvironmentManifest::parse(input, "test.toml").expect("valid fixture")
    }

    #[test]
    fn strict_parser_reports_source_and_span() {
        let error = EnvironmentManifest::parse(
            "schema_version=1\nname='x'\nimage='x:1'\nunexpected=true\n",
            "mine.toml",
        )
        .expect_err("unknown field must fail");
        match error {
            EnvironmentError::Parse {
                source_label, span, ..
            } => {
                assert_eq!(source_label, "mine.toml");
                assert!(span.is_some());
            }
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn rejects_mutable_or_malformed_image_references() {
        for image in [
            "example/image",
            "example/image:latest",
            "example/image:",
            "example/image@sha256:1234",
        ] {
            let input = format!("name='custom'\nimage={image:?}\n");
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_err());
        }
        assert!(
            EnvironmentManifest::parse("name='custom'\nimage='example/image:1.2.3'\n", "x").is_ok()
        );
    }

    #[test]
    fn inheritance_merges_maps_resources_and_removals() {
        let mut registry = EnvironmentRegistry::new();
        registry
            .insert(
                manifest(
                    r#"
                    name = "base"
                    image = "example/base:1"
                    workdir = "/workspace"
                    allow_hosts = ["example.com"]
                    env = { A = "one", B = "base" }
                    [[caches]]
                    id = "cargo"
                    target = "/cache/cargo"
                    [[ports]]
                    id = "dev"
                    container = 3000
                    "#,
                ),
                ManifestSource::built_in("base"),
            )
            .expect("insert base");
        registry
            .insert(
                manifest(
                    r#"
                    name = "child"
                    extends = "base"
                    markers = ["Cargo.toml"]
                    allow_hosts = ["crates.io"]
                    env = { B = "child" }
                    [[caches]]
                    id = "cargo"
                    target = "/new/cargo"
                    scope = "shared"
                    [[ports]]
                    id = "dev"
                    container = 1
                    remove = true
                    "#,
                ),
                ManifestSource::file(Path::new("child.toml")),
            )
            .expect("insert child");
        let resolved = registry.resolve("child").expect("resolve");
        assert_eq!(resolved.image.as_deref(), Some("example/base:1"));
        assert_eq!(resolved.ancestry, ["base", "child"]);
        assert_eq!(resolved.env["A"], "one");
        assert_eq!(resolved.env["B"], "child");
        assert_eq!(resolved.allow_hosts, ["crates.io"]);
        assert_eq!(resolved.caches[0].scope, CacheScope::Shared);
        assert!(resolved.ports.is_empty());
    }

    #[test]
    fn explicit_empty_arrays_clear_inherited_values_while_omission_inherits() {
        let base = manifest(
            r#"
            name = "base"
            image = "example/base:1"
            markers = ["package.json"]
            prepare = [{ program = "npm", args = ["install"] }]
            allow_hosts = ["registry.npmjs.org"]
            "#,
        );
        let omitted = manifest("name='omitted'\nextends='base'");
        let cleared =
            manifest("name='cleared'\nextends='base'\nmarkers=[]\nprepare=[]\nallow_hosts=[]");
        assert_eq!(omitted.markers, None);
        assert_eq!(omitted.prepare, None);
        assert_eq!(omitted.allow_hosts, None);
        assert_eq!(cleared.markers, Some(Vec::new()));
        assert_eq!(cleared.prepare, Some(Vec::new()));
        assert_eq!(cleared.allow_hosts, Some(Vec::new()));

        let mut registry = EnvironmentRegistry::new();
        for environment in [base, omitted, cleared] {
            let name = environment.name.clone();
            registry
                .insert(environment, ManifestSource::built_in(&name))
                .expect("insert");
        }
        let inherited = registry.resolve("omitted").expect("inherit");
        assert_eq!(inherited.markers, [PathBuf::from("package.json")]);
        assert_eq!(inherited.prepare.len(), 1);
        assert_eq!(inherited.allow_hosts, ["registry.npmjs.org"]);
        let cleared = registry.resolve("cleared").expect("clear");
        assert!(cleared.markers.is_empty());
        assert!(cleared.prepare.is_empty());
        assert!(cleared.allow_hosts.is_empty());
    }

    #[test]
    fn rejects_plaintext_secrets_in_build_arguments() {
        for build_args in [
            "OPENAI_API_KEY='sk-plaintext'",
            "TOKEN_ENV_VAR='sk-plaintext'",
            "OPTIONS='TOKEN=plaintext'",
        ] {
            let input = format!(
                "name='custom'\n[build]\ncontext='.'\ndockerfile='Dockerfile'\n[build.args]\n{build_args}"
            );
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_err());
        }

        let safe = r#"
            name = "custom"
            [build]
            context = "."
            dockerfile = "Dockerfile"
            [build.args]
            OPENAI_API_KEY = "${OPENAI_API_KEY}"
            TOKEN_ENV_VAR = "OPENAI_API_KEY"
        "#;
        assert!(EnvironmentManifest::parse(safe, "custom.toml").is_ok());
    }

    #[test]
    fn rejects_plaintext_secrets_in_prepare_argv() {
        for prepare in [
            "[{program='tool',args=['--token','plaintext']}]",
            "[{program='tool',args=['--token-env-var','not an env name']}]",
            "[{program='tool',args=['--token-env-var=sk-plaintext']}]",
            "[{program='env',args=['TOKEN=plaintext','tool']}]",
            "[{program='sh',args=['-c','tool --api-key plaintext']}]",
            "[{program='curl',args=['-H','Authorization: Bearer plaintext']}]",
        ] {
            let input = format!("name='custom'\nimage='example/custom:1'\nprepare={prepare}");
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_err());
        }

        for prepare in [
            "[{program='tool',args=['--token','$TOKEN']}]",
            "[{program='tool',args=['--token-env-var','TOKEN']}]",
            "[{program='curl',args=['-H','Authorization: Bearer ${TOKEN}']}]",
        ] {
            let input = format!("name='custom'\nimage='example/custom:1'\nprepare={prepare}");
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_ok());
        }
    }

    #[test]
    fn host_rules_require_valid_dns_idna_ip_and_ports() {
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
            let input = format!("name='custom'\nimage='example/custom:1'\nallow_hosts=[{rule:?}]");
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_err());
        }
        for rule in [
            "faß.de",
            "*.example.com:*",
            "127.0.0.1:443",
            "[2001:db8::1]:443",
        ] {
            let input = format!("name='custom'\nimage='example/custom:1'\nallow_hosts=[{rule:?}]");
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_ok());
        }
    }

    #[test]
    fn host_services_validate_targets_aliases_and_both_ports_at_parse_time() {
        for declaration in [
            "host='bad_label.example',port=443",
            "host='example.com:443',port=443",
            "host='*.example.com',port=443",
            "host='example.com',port=443,container_port=0",
            "host='example.com',port=443,container_host='*.service.test'",
            "host='example.com',port=443,container_host='192.0.2.1'",
            "host='example.com',port=443,container_host='::1'",
        ] {
            let input = format!(
                "name='custom'\nimage='example/custom:1'\nhost_services=[{{id='service',{declaration}}}]"
            );
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_err());
        }

        for declaration in [
            "host='host.containers.internal',port=443,container_host='database.host'",
            "host='faß.de',port=443,container_host='127.0.0.1'",
            "host='2001:db8::1',port=443",
        ] {
            let input = format!(
                "name='custom'\nimage='example/custom:1'\nhost_services=[{{id='service',{declaration}}}]"
            );
            assert!(EnvironmentManifest::parse(&input, "custom.toml").is_ok());
        }
    }

    #[test]
    fn detects_cycles() {
        let mut registry = EnvironmentRegistry::new();
        for (name, parent) in [("a", "b"), ("b", "c"), ("c", "a")] {
            registry
                .insert(
                    manifest(&format!("name='{name}'\nextends='{parent}'")),
                    ManifestSource::built_in(name),
                )
                .expect("insert");
        }
        assert!(matches!(
            registry.resolve("a"),
            Err(EnvironmentError::InheritanceCycle(message)) if message == "a -> b -> c -> a"
        ));
    }

    #[test]
    fn marker_detection_prefers_most_specific() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("Cargo.toml"), "").expect("marker");
        std::fs::create_dir(directory.path().join("src")).expect("src");
        let mut registry = EnvironmentRegistry::new();
        registry
            .insert(
                manifest("name='generic'\nimage='base:1'"),
                ManifestSource::built_in("generic"),
            )
            .expect("generic");
        registry
            .insert(
                manifest("name='rust'\nextends='generic'\nmarkers=['Cargo.toml','src']"),
                ManifestSource::built_in("rust"),
            )
            .expect("rust");
        assert_eq!(
            registry
                .detect(directory.path())
                .expect("detect")
                .as_deref(),
            Some("rust")
        );
    }

    #[test]
    fn rejects_shell_like_empty_commands_and_bad_mounts() {
        assert!(
            EnvironmentManifest::parse("name='x'\nimage='x:1'\nprepare=[{program=''}]", "x")
                .is_err()
        );
        assert!(
            EnvironmentManifest::parse(
                "name='x'\nimage='x:1'\nmounts=[{id='m',target='relative'}]",
                "x"
            )
            .is_err()
        );
    }

    #[test]
    fn minimal_removals_and_explicit_port_host_rules_are_valid() {
        let child = manifest(
            r#"
            name = "child"
            extends = "base"
            allow_hosts = ["api.example.com:443", "*.example.com:*", "[2001:db8::1]:8443"]
            [[mounts]]
            id = "old-mount"
            remove = true
            [[caches]]
            id = "old-cache"
            remove = true
            [[ports]]
            id = "old-port"
            remove = true
            [[host_services]]
            id = "old-service"
            remove = true
            "#,
        );
        assert_eq!(child.mounts[0].target, PathBuf::new());
        assert_eq!(child.ports[0].container, 0);
    }

    #[test]
    fn unknown_environment_field_has_a_suggestion() {
        let error = EnvironmentManifest::parse(
            "name='x'\nimage='x:1'\nallow_hsots=['example.com']",
            "custom.toml",
        )
        .expect_err("typo");
        assert!(matches!(
            error,
            EnvironmentError::Parse { suggestion, .. }
                if suggestion.as_deref() == Some("allow_hosts")
        ));
    }
}
