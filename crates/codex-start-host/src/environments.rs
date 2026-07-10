//! Built-in and user-defined environment loading and image planning.

use std::{
    collections::BTreeMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

use crate::{
    assets, content_hash,
    error::{HostError, Result},
    paths::AppPaths,
    runtime::{BuildRequest, MountKind, MountRequest, PublishRequest, Runtime},
};
use codex_start_core::{
    CacheScope, EnvironmentManifest, EnvironmentRegistry, ManifestSource, ResolvedEnvironment,
};
use serde::Serialize;

const BUILT_INS: [(&str, &str); 4] = [
    (
        "generic",
        include_str!("../../../assets/environments/generic.toml"),
    ),
    ("web", include_str!("../../../assets/environments/web.toml")),
    ("uv", include_str!("../../../assets/environments/uv.toml")),
    (
        "rust",
        include_str!("../../../assets/environments/rust.toml"),
    ),
];

/// Loaded environment definitions and source locations.
#[derive(Clone, Debug)]
pub struct EnvironmentCatalog {
    registry: EnvironmentRegistry,
    source_paths: BTreeMap<String, PathBuf>,
    assets_root: PathBuf,
    sidecar_build_args: BTreeMap<String, String>,
}

/// Container resources generated for a resolved environment.
#[derive(Clone, Debug)]
pub struct EnvironmentResources {
    /// Filesystem and cache mounts.
    pub mounts: Vec<MountRequest>,
    /// Default loopback-only port publications.
    pub ports: Vec<PublishRequest>,
    /// Environment-provided variables.
    pub env: BTreeMap<String, std::ffi::OsString>,
    /// Preparation commands consumed by the Rust init helper.
    pub prepare: Vec<codex_start_proxy::container_init::CommandSpec>,
}

/// A resolved environment together with the source of every contributed field.
#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentReport {
    pub environment: ResolvedEnvironment,
    pub provenance: BTreeMap<String, ManifestSource>,
}

impl EnvironmentCatalog {
    /// Load embedded built-ins followed by user overrides.
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let mut registry = EnvironmentRegistry::new();
        let assets_root = assets::materialize(paths)?;
        let built_in_dir = assets_root.join("assets/environments");
        let lock = load_lock_overrides(paths)?;
        let mut source_paths = BTreeMap::new();
        for (name, contents) in BUILT_INS {
            let mut manifest = EnvironmentManifest::parse(contents, format!("built-in:{name}"))
                .map_err(environment_error)?;
            lock.apply_to_environment(&mut manifest);
            registry
                .insert(manifest, ManifestSource::built_in(name))
                .map_err(environment_error)?;
            source_paths.insert(name.to_owned(), built_in_dir.join(format!("{name}.toml")));
        }
        if paths.environments_dir().is_dir() {
            let mut files = fs::read_dir(paths.environments_dir())
                .map_err(|source| HostError::io(paths.environments_dir(), source))?
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "toml")
                })
                .collect::<Vec<_>>();
            files.sort();
            for path in files {
                let contents =
                    fs::read_to_string(&path).map_err(|source| HostError::io(&path, source))?;
                let manifest = EnvironmentManifest::parse(&contents, path.display().to_string())
                    .map_err(environment_error)?;
                let name = manifest.name.clone();
                registry
                    .replace(manifest, ManifestSource::file(&path))
                    .map_err(environment_error)?;
                source_paths.insert(name, path);
            }
        }
        Ok(Self {
            registry,
            source_paths,
            assets_root,
            sidecar_build_args: lock.sidecar_args(),
        })
    }

    /// Root of the immutable build bundle embedded in this executable.
    pub fn assets_root(&self) -> &Path {
        &self.assets_root
    }

    /// Locked build arguments needed by the sidecar image.
    pub fn sidecar_build_args(&self) -> &BTreeMap<String, String> {
        &self.sidecar_build_args
    }

    /// Sorted environment names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.registry.names()
    }

    /// Resolve inheritance and validate an environment.
    pub fn resolve(&self, name: &str) -> Result<ResolvedEnvironment> {
        self.registry.resolve(name).map_err(environment_error)
    }

    /// Resolve inheritance and report field/resource provenance.
    pub fn report(&self, name: &str) -> Result<EnvironmentReport> {
        let environment = self.resolve(name)?;
        let mut provenance = BTreeMap::new();
        for ancestor in &environment.ancestry {
            let manifest = self.registry.manifest(ancestor).ok_or_else(|| {
                HostError::Config(format!("manifest for environment {ancestor:?} disappeared"))
            })?;
            let source = self.registry.source(ancestor).ok_or_else(|| {
                HostError::Config(format!("source for environment {ancestor:?} disappeared"))
            })?;
            record_manifest_provenance(manifest, source, &mut provenance)?;
        }
        provenance
            .entry("workdir".to_owned())
            .or_insert_with(|| ManifestSource::built_in("default"));
        Ok(EnvironmentReport {
            environment,
            provenance,
        })
    }

    /// Detect the most specific marker-matching environment.
    pub fn detect(&self, root: &Path) -> Result<Option<String>> {
        self.registry.detect(root).map_err(environment_error)
    }

    /// Resolve environment-relative build paths.
    pub fn build_request(
        &self,
        environment: &ResolvedEnvironment,
        image: String,
        no_cache: bool,
    ) -> Result<Option<BuildRequest>> {
        let Some(build) = &environment.build else {
            return Ok(None);
        };
        let owner = self.build_owner(environment)?;
        let manifest = self.source_paths.get(owner).ok_or_else(|| {
            HostError::Config(format!(
                "source path for environment {owner:?} is unavailable"
            ))
        })?;
        let directory = manifest.parent().ok_or_else(|| HostError::UnsafePath {
            path: manifest.clone(),
            reason: "manifest has no parent".to_owned(),
        })?;
        let context = resolve_relative(directory, &build.context)?;
        let dockerfile = resolve_relative(&context, &build.dockerfile)?;
        if !dockerfile.is_file() {
            return Err(HostError::NotFound(format!(
                "Dockerfile {}",
                dockerfile.display()
            )));
        }
        Ok(Some(BuildRequest {
            image,
            context,
            dockerfile,
            target: build.target.clone(),
            build_args: build.args.clone(),
            no_cache,
        }))
    }

    /// Compute the immutable local image tag for a fully resolved environment.
    pub fn image_tag(&self, environment: &ResolvedEnvironment) -> Result<String> {
        if let Some(image) = &environment.image {
            return Ok(image.clone());
        }
        let encoded = serde_json::to_vec(environment)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&encoded);
        hasher.update(std::env::consts::ARCH.as_bytes());
        if let Ok(owner) = self.build_owner(environment)
            && let Some(source) = self.source_paths.get(owner)
        {
            let directory = source.parent().unwrap_or(source);
            if let Some(build) = &environment.build {
                let context = resolve_relative(directory, &build.context)?;
                content_hash::hash_tree(&context, &context, &mut hasher, ignored_build_path)?;
            }
        }
        let digest = hasher.finalize().to_hex();
        Ok(format!(
            "codex-start-env-{}:{}",
            environment.name,
            &digest[..16]
        ))
    }

    /// Ensure a build-backed image exists, rebuilding when requested.
    pub fn ensure_image(
        &self,
        runtime: &Runtime,
        environment: &ResolvedEnvironment,
        rebuild: bool,
        no_cache: bool,
        pull: bool,
    ) -> Result<String> {
        let image = self.image_tag(environment)?;
        if pull && environment.build.is_some() {
            let built_in = self
                .registry
                .source(&environment.name)
                .is_some_and(|source| source.built_in);
            if !built_in {
                return Err(HostError::Config(
                    "--pull supports shipped environments or a custom immutable `image`; custom builds use --rebuild"
                        .to_owned(),
                ));
            }
            let registry = std::env::var("CODEX_START_IMAGE_REGISTRY")
                .unwrap_or_else(|_| "ghcr.io/cofob".to_owned());
            let remote = format!(
                "{}/codex-start-{}:v{}",
                registry.trim_end_matches('/'),
                environment.name,
                env!("CARGO_PKG_VERSION")
            );
            if runtime.pull(&remote)? != 0 {
                return Err(HostError::Runtime(format!("pulling {remote} failed")));
            }
            return Ok(remote);
        }
        if environment.build.is_some() && (rebuild || !runtime.image_exists(&image)?) {
            let request = self
                .build_request(environment, image.clone(), no_cache)?
                .ok_or_else(|| HostError::Config("environment build disappeared".to_owned()))?;
            let status = runtime.build(&request)?;
            if status != 0 {
                return Err(HostError::Runtime(format!(
                    "image build for {} exited with {status}",
                    environment.name
                )));
            }
        } else if environment.image.is_some() && (pull || !runtime.image_exists(&image)?) {
            let status = runtime.pull(&image)?;
            if status != 0 {
                return Err(HostError::Runtime(format!(
                    "pulling {image} exited with {status}"
                )));
            }
        }
        Ok(image)
    }

    /// Materialize environment mounts, volumes, ports, and preparation data.
    pub fn resources(
        &self,
        environment: &ResolvedEnvironment,
        project_id: &str,
        run_id: &str,
    ) -> Result<EnvironmentResources> {
        let mut mounts = environment
            .mounts
            .iter()
            .map(|mount| {
                let kind = match mount.kind {
                    codex_start_core::MountKind::Bind => MountKind::Bind,
                    codex_start_core::MountKind::Volume => MountKind::Volume,
                    codex_start_core::MountKind::Tmpfs => MountKind::Tmpfs,
                };
                let source_value = match mount.source.as_ref() {
                    Some(value) if kind == MountKind::Bind && value.is_relative() => Some(
                        self.mount_resource_directory(environment, &mount.id)?
                            .join(value)
                            .into_os_string(),
                    ),
                    Some(value) => Some(value.as_os_str().to_owned()),
                    None => None,
                };
                Ok(MountRequest {
                    kind,
                    source: source_value,
                    target: mount.target.clone(),
                    read_only: mount.read_only,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        mounts.extend(environment.caches.iter().map(|cache| {
            let scope = match cache.scope {
                CacheScope::Shared => "shared".to_owned(),
                CacheScope::Project => project_id.to_owned(),
                CacheScope::Environment => environment.name.clone(),
                CacheScope::Run => run_id.to_owned(),
            };
            MountRequest {
                kind: MountKind::Volume,
                source: Some(cache_volume_name(&scope, &cache.id).into()),
                target: cache.target.clone(),
                read_only: cache.read_only,
            }
        }));
        let ports = environment
            .ports
            .iter()
            .map(|port| {
                Ok(PublishRequest {
                    host_ip: port.host_ip.parse::<IpAddr>().map_err(|error| {
                        HostError::Config(format!("invalid environment host IP: {error}"))
                    })?,
                    host_port: port.host.unwrap_or(port.container),
                    container_port: port.container,
                    protocol: match port.protocol {
                        codex_start_core::PortProtocol::Tcp => "tcp",
                        codex_start_core::PortProtocol::Udp => "udp",
                    }
                    .to_owned(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let prepare = environment
            .prepare
            .iter()
            .map(|command| codex_start_proxy::container_init::CommandSpec {
                program: command.program.clone(),
                args: command.args.clone(),
                env: BTreeMap::new(),
                cwd: None,
            })
            .collect::<Vec<_>>();
        Ok(EnvironmentResources {
            mounts,
            ports,
            env: environment
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.into()))
                .collect(),
            prepare,
        })
    }

    fn build_owner<'a>(&self, environment: &'a ResolvedEnvironment) -> Result<&'a str> {
        environment
            .ancestry
            .iter()
            .rev()
            .find(|name| {
                self.registry
                    .manifest(name)
                    .is_some_and(|manifest| manifest.build.is_some() || manifest.image.is_some())
            })
            .map(String::as_str)
            .ok_or_else(|| {
                HostError::Config(format!(
                    "environment {:?} has no image/build owner",
                    environment.name
                ))
            })
    }

    fn mount_resource_directory(
        &self,
        environment: &ResolvedEnvironment,
        id: &str,
    ) -> Result<PathBuf> {
        let owner = environment
            .ancestry
            .iter()
            .rev()
            .find(|name| {
                self.registry
                    .manifest(name)
                    .is_some_and(|manifest| manifest.mounts.iter().any(|item| item.id == id))
            })
            .ok_or_else(|| HostError::Config(format!("resource {id:?} has no source manifest")))?;
        self.source_paths
            .get(owner)
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| HostError::Config(format!("source path for {owner:?} is unavailable")))
    }
}

fn record_manifest_provenance(
    manifest: &EnvironmentManifest,
    source: &ManifestSource,
    output: &mut BTreeMap<String, ManifestSource>,
) -> Result<()> {
    output.insert("name".to_owned(), source.clone());
    output.insert("schema_version".to_owned(), source.clone());
    if manifest.image.is_some() || manifest.build.is_some() {
        remove_provenance_prefix(output, "image");
        remove_provenance_prefix(output, "build");
    }
    for (path, present) in [
        ("description", manifest.description.is_some()),
        ("image", manifest.image.is_some()),
        ("build", manifest.build.is_some()),
        ("workdir", manifest.workdir.is_some()),
        ("user", manifest.user.is_some()),
        ("markers", manifest.markers.is_some()),
        ("prepare", manifest.prepare.is_some()),
        ("allow_hosts", manifest.allow_hosts.is_some()),
    ] {
        if present {
            output.insert(path.to_owned(), source.clone());
        }
    }
    let settings = toml::Value::try_from(&manifest.settings)
        .map_err(|error| HostError::Serialization(error.to_string()))?;
    record_toml_provenance("settings", &settings, source, output);
    if let Some(build) = &manifest.build {
        let build = toml::Value::try_from(build)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        record_toml_provenance("build", &build, source, output);
    }
    for key in manifest.env.keys() {
        output.insert(format!("env.{}", toml_key_segment(key)), source.clone());
    }
    for key in manifest.secret_refs.keys() {
        output.insert(
            format!("secret_refs.{}", toml_key_segment(key)),
            source.clone(),
        );
    }
    for item in &manifest.mounts {
        record_resource("mounts", &item.id, item.remove, source, output);
    }
    for item in &manifest.caches {
        record_resource("caches", &item.id, item.remove, source, output);
    }
    for item in &manifest.ports {
        record_resource("ports", &item.id, item.remove, source, output);
    }
    for item in &manifest.host_services {
        record_resource("host_services", &item.id, item.remove, source, output);
    }
    Ok(())
}

fn remove_provenance_prefix(output: &mut BTreeMap<String, ManifestSource>, prefix: &str) {
    let nested = format!("{prefix}.");
    output.retain(|path, _| path != prefix && !path.starts_with(&nested));
}

fn record_toml_provenance(
    prefix: &str,
    value: &toml::Value,
    source: &ManifestSource,
    output: &mut BTreeMap<String, ManifestSource>,
) {
    if let toml::Value::Table(table) = value {
        for (key, value) in table {
            record_toml_provenance(
                &format!("{prefix}.{}", toml_key_segment(key)),
                value,
                source,
                output,
            );
        }
    } else {
        output.insert(prefix.to_owned(), source.clone());
    }
}

fn record_resource(
    kind: &str,
    id: &str,
    remove: bool,
    source: &ManifestSource,
    output: &mut BTreeMap<String, ManifestSource>,
) {
    let path = format!("{kind}.{}", toml_key_segment(id));
    if remove {
        output.remove(&path);
    } else {
        output.insert(path, source.clone());
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

#[derive(Clone, Debug, Default)]
struct LockOverrides {
    common: BTreeMap<String, String>,
    uv: BTreeMap<String, String>,
    sidecar: BTreeMap<String, String>,
}

impl LockOverrides {
    fn apply_to_environment(&self, manifest: &mut EnvironmentManifest) {
        let Some(build) = &mut manifest.build else {
            return;
        };
        build.args.extend(self.common.clone());
        if manifest.name == "uv" {
            build.args.extend(self.uv.clone());
        }
    }

    fn sidecar_args(&self) -> BTreeMap<String, String> {
        self.sidecar.clone()
    }
}

fn load_lock_overrides(paths: &AppPaths) -> Result<LockOverrides> {
    let path = paths.config.join("images.lock.toml");
    if !path.is_file() {
        return Ok(LockOverrides::default());
    }
    let contents = fs::read_to_string(&path).map_err(|source| HostError::io(&path, source))?;
    let value = contents
        .parse::<toml::Value>()
        .map_err(|error| HostError::Config(format!("{}: {error}", path.display())))?;
    if value
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        != Some(1)
    {
        return Err(HostError::Config(format!(
            "{} has an unsupported or missing schema_version",
            path.display()
        )));
    }
    let node = locked_image(&value, "node")?;
    let rust = locked_image(&value, "rust")?;
    let debian = locked_image(&value, "debian")?;
    let codex_refresh = lock_string(&value, &["generated_at"])?;
    let uv_version = lock_string(&value, &["artifacts", "uv", "version"])?;
    let uv_amd64 = lock_string(
        &value,
        &["artifacts", "uv", "platforms", "linux/amd64", "sha256"],
    )?;
    let uv_arm64 = lock_string(
        &value,
        &["artifacts", "uv", "platforms", "linux/arm64", "sha256"],
    )?;
    let just_version = lock_string(&value, &["artifacts", "just", "version"])?;
    let just_amd64 = lock_string(
        &value,
        &["artifacts", "just", "platforms", "linux/amd64", "sha256"],
    )?;
    let just_arm64 = lock_string(
        &value,
        &["artifacts", "just", "platforms", "linux/arm64", "sha256"],
    )?;
    Ok(LockOverrides {
        common: BTreeMap::from([
            ("NODE_IMAGE".to_owned(), node),
            ("RUST_IMAGE".to_owned(), rust.clone()),
            ("CODEX_REFRESH".to_owned(), codex_refresh),
        ]),
        uv: BTreeMap::from([
            ("UV_VERSION".to_owned(), uv_version),
            ("UV_SHA256_AMD64".to_owned(), uv_amd64),
            ("UV_SHA256_ARM64".to_owned(), uv_arm64),
            ("JUST_VERSION".to_owned(), just_version),
            ("JUST_SHA256_AMD64".to_owned(), just_amd64),
            ("JUST_SHA256_ARM64".to_owned(), just_arm64),
        ]),
        sidecar: BTreeMap::from([
            ("RUST_IMAGE".to_owned(), rust),
            ("DEBIAN_IMAGE".to_owned(), debian),
        ]),
    })
}

fn locked_image(value: &toml::Value, name: &str) -> Result<String> {
    let repository = lock_string(value, &["images", name, "repository"])?;
    let tag = lock_string(value, &["images", name, "tag"])?;
    let digest = lock_string(value, &["images", name, "index_digest"])?;
    Ok(format!("{repository}:{tag}@{digest}"))
}

fn lock_string(value: &toml::Value, path: &[&str]) -> Result<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment).ok_or_else(|| {
            HostError::Config(format!("user image lock is missing {}", path.join(".")))
        })?;
    }
    current.as_str().map(str::to_owned).ok_or_else(|| {
        HostError::Config(format!(
            "user image lock field {} must be a string",
            path.join(".")
        ))
    })
}

fn resolve_relative(parent: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        parent.join(path)
    };
    joined
        .canonicalize()
        .map_err(|source| HostError::io(&joined, source))
}

fn ignored_build_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | "target" | ".DS_Store")
        )
    })
}

fn sanitize(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            result.push(character.to_ascii_lowercase());
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches('-').to_owned()
}

fn cache_volume_name(scope: &str, id: &str) -> String {
    let readable = sanitize(&format!("{scope}-{id}"));
    let readable = &readable[..readable.len().min(64)];
    let mut hasher = blake3::Hasher::new();
    for value in [scope, id] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize().to_hex();
    format!("codex-start-cache-{readable}-{}", &digest[..32])
}

fn environment_error(error: impl std::fmt::Display) -> HostError {
    HostError::Config(error.to_string())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{collections::BTreeMap, fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    use codex_start_core::{EnvironmentManifest, EnvironmentRegistry, ManifestSource};

    use super::{EnvironmentCatalog, cache_volume_name};
    use crate::paths::AppPaths;

    #[test]
    fn resolves_all_built_in_environments() {
        let root = tempfile::tempdir().expect("root");
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let catalog = EnvironmentCatalog::load(&paths).expect("catalog");
        assert_eq!(
            catalog.names().collect::<Vec<_>>(),
            ["generic", "rust", "uv", "web"]
        );
        for name in catalog.names() {
            let environment = catalog.resolve(name).expect("resolve");
            assert!(environment.build.is_some() || environment.image.is_some());
            let request = catalog
                .build_request(&environment, format!("test/{name}:locked"), false)
                .expect("build paths")
                .expect("built-in build request");
            assert!(request.context.is_dir());
            assert!(request.dockerfile.is_file());
        }
    }

    #[test]
    fn environment_report_tracks_inherited_field_and_resource_sources() {
        let root = tempfile::tempdir().expect("root");
        let paths = AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let catalog = EnvironmentCatalog::load(&paths).expect("catalog");
        let report = catalog.report("web").expect("report");
        assert_eq!(
            report.provenance["env.NPM_CONFIG_CACHE"].display,
            "built-in:generic"
        );
        assert_eq!(
            report.provenance["ports.development"].display,
            "built-in:web"
        );
        assert_eq!(report.provenance["build.target"].display, "built-in:web");
        assert_eq!(report.environment.ancestry, ["generic", "web"]);
    }

    #[test]
    fn cache_volume_names_preserve_readability_without_sanitization_collisions() {
        let dotted = cache_volume_name("project.id", "a.b");
        let dashed = cache_volume_name("project-id", "a-b");
        assert!(dotted.starts_with("codex-start-cache-project-id-a-b-"));
        assert!(dashed.starts_with("codex-start-cache-project-id-a-b-"));
        assert_ne!(dotted, dashed);
        assert_eq!(dotted, cache_volume_name("project.id", "a.b"));
    }

    #[test]
    #[cfg(unix)]
    fn environment_tag_tracks_entry_type_target_and_mode() {
        let root = tempfile::tempdir().expect("root");
        let (catalog, environment) = custom_catalog(root.path());
        let source = root.path().join("context/tool");
        let same = root.path().join("context/same");
        let other = root.path().join("context/other");
        for path in [&source, &same, &other] {
            fs::write(path, "payload\n").expect("source");
        }
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).expect("mode");
        let regular = catalog.image_tag(&environment).expect("regular tag");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("mode");
        let executable = catalog.image_tag(&environment).expect("executable tag");
        assert_ne!(regular, executable);

        fs::remove_file(&source).expect("remove regular");
        std::os::unix::fs::symlink("same", &source).expect("symlink");
        let first_target = catalog.image_tag(&environment).expect("first symlink tag");
        assert_ne!(regular, first_target);
        fs::remove_file(&source).expect("remove symlink");
        std::os::unix::fs::symlink("other", &source).expect("symlink");
        let second_target = catalog.image_tag(&environment).expect("second symlink tag");
        assert_ne!(first_target, second_target);
    }

    #[test]
    #[cfg(unix)]
    fn environment_tag_rejects_symlinks_outside_the_context() {
        let root = tempfile::tempdir().expect("root");
        let (catalog, environment) = custom_catalog(root.path());
        fs::write(root.path().join("outside"), "payload\n").expect("outside");
        std::os::unix::fs::symlink("../outside", root.path().join("context/tool"))
            .expect("symlink");

        let error = catalog.image_tag(&environment).expect_err("unsafe symlink");
        assert!(matches!(error, crate::error::HostError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    fn custom_catalog(root: &Path) -> (EnvironmentCatalog, codex_start_core::ResolvedEnvironment) {
        let context = root.join("context");
        fs::create_dir_all(&context).expect("context");
        fs::write(context.join("Dockerfile"), "FROM scratch\n").expect("Dockerfile");
        let manifest_path = root.join("custom.toml");
        let manifest = EnvironmentManifest::parse(
            r#"
                schema_version = 1
                name = "custom"

                [build]
                context = "context"
                dockerfile = "Dockerfile"
            "#,
            manifest_path.display().to_string(),
        )
        .expect("manifest");
        fs::write(&manifest_path, "manifest source\n").expect("manifest path");
        let mut registry = EnvironmentRegistry::new();
        registry
            .insert(manifest, ManifestSource::file(&manifest_path))
            .expect("registry");
        let environment = registry.resolve("custom").expect("resolve");
        let catalog = EnvironmentCatalog {
            registry,
            source_paths: BTreeMap::from([("custom".to_owned(), manifest_path)]),
            assets_root: root.to_path_buf(),
            sidecar_build_args: BTreeMap::new(),
        };
        (catalog, environment)
    }
}
