//! Reproducible image and release maintenance commands.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use codex_start_core::ContainerPath;
use flate2::{Compression, GzBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::{Builder as TarBuilder, EntryType as TarEntryType, Header as TarHeader, HeaderMode};
use thiserror::Error;

const REQUIRED_ENVIRONMENTS: [&str; 4] = ["generic", "web", "uv", "rust"];
const REQUIRED_PLATFORMS: [&str; 2] = ["linux/amd64", "linux/arm64"];

#[derive(Debug, Parser)]
#[command(about = "Repository maintenance commands", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate every checked-in manifest, pin, and Docker build argument.
    Validate {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Validate only the image and artifact lock.
    ValidateLock {
        #[arg(long, default_value = "assets/images.lock.toml")]
        lock: PathBuf,
    },
    /// Validate built-in environment manifests and inheritance.
    ValidateManifests {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Print immutable Docker build arguments derived from the lock.
    BuildArgs {
        #[arg(long, default_value = "assets/images.lock.toml")]
        lock: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Lines)]
        format: OutputFormat,
    },
    /// Calculate the content-addressed local image tag for an environment.
    ImageTag {
        environment: String,
        #[arg(value_enum)]
        architecture: Architecture,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        version: String,
    },
    /// Calculate or verify one file's SHA-256 digest.
    Checksum {
        file: PathBuf,
        #[arg(long)]
        expected: Option<String>,
    },
    /// Create a deterministic tar.gz or ZIP release archive.
    ReleaseArchive {
        /// Directory whose contents will be archived.
        directory: PathBuf,
        /// Destination, which must end in `.tar.gz` and must not already exist.
        output: PathBuf,
        /// Top-level directory stored in the archive.
        #[arg(long)]
        prefix: PathBuf,
        /// Timestamp for both tar entries and the gzip header.
        #[arg(long, env = "SOURCE_DATE_EPOCH")]
        source_date_epoch: u32,
        /// Relative regular-file path to store with mode 0755 (repeatable).
        #[arg(long = "executable", value_name = "RELATIVE_PATH")]
        executables: Vec<PathBuf>,
    },
    /// Create a deterministic SHA256SUMS file for a release directory.
    ReleaseChecksums {
        directory: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Verify every entry in a SHA256SUMS file.
    VerifyChecksums {
        manifest: PathBuf,
        #[arg(long)]
        base: Option<PathBuf>,
    },
    /// Generate the schema-versioned release artifact manifest.
    ReleaseManifest {
        directory: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Validate a release artifact manifest and all referenced files.
    ValidateReleaseManifest {
        manifest: PathBuf,
        #[arg(long)]
        base: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Lines,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Architecture {
    Amd64,
    Arm64,
}

impl Architecture {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

#[derive(Debug, Error)]
enum Error {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to publish {path}: {source}")]
    Persist {
        path: PathBuf,
        source: tempfile::PersistError,
    },
    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to process ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("checksum mismatch for {path}: expected {expected}, calculated {actual}")]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

type Result<T> = std::result::Result<T, Error>;

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Validate { root } => {
            validate_repository(&root)?;
            println!("validated lock, four environment manifests, and Docker pins");
        }
        Command::ValidateLock { lock } => {
            let _ = load_and_validate_lock(&lock)?;
            println!("validated {}", lock.display());
        }
        Command::ValidateManifests { root } => {
            let lock_path = root.join("assets/images.lock.toml");
            let lock = load_and_validate_lock(&lock_path)?;
            validate_manifests(&root, &lock)?;
            println!("validated {}", root.join("assets/environments").display());
        }
        Command::BuildArgs { lock, format } => {
            let lock = load_and_validate_lock(&lock)?;
            print_build_args(&lock, format)?;
        }
        Command::ImageTag {
            environment,
            architecture,
            root,
            version,
        } => println!(
            "{}",
            image_tag(&root, &environment, architecture, &version)?
        ),
        Command::Checksum { file, expected } => {
            let actual = sha256_file(&file)?;
            if let Some(expected) = expected {
                let expected = expected.strip_prefix("sha256:").unwrap_or(&expected);
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err(Error::ChecksumMismatch {
                        path: file,
                        expected: expected.to_owned(),
                        actual,
                    });
                }
            }
            println!("{actual}");
        }
        Command::ReleaseArchive {
            directory,
            output,
            prefix,
            source_date_epoch,
            executables,
        } => {
            write_release_archive(
                &directory,
                &output,
                &prefix,
                source_date_epoch,
                &executables,
            )?;
            println!("wrote {}", output.display());
        }
        Command::ReleaseChecksums { directory, output } => {
            let output = output.unwrap_or_else(|| directory.join("SHA256SUMS"));
            write_release_checksums(&directory, &output)?;
            println!("wrote {}", output.display());
        }
        Command::VerifyChecksums { manifest, base } => {
            let base = base.unwrap_or_else(|| {
                manifest
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf()
            });
            verify_checksums(&manifest, &base)?;
            println!("verified {}", manifest.display());
        }
        Command::ReleaseManifest {
            directory,
            version,
            output,
        } => {
            let output = output.unwrap_or_else(|| directory.join("release-manifest.json"));
            write_release_manifest(&directory, &output, &version)?;
            println!("wrote {}", output.display());
        }
        Command::ValidateReleaseManifest { manifest, base } => {
            let base = base.unwrap_or_else(|| {
                manifest
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf()
            });
            let release = load_release_manifest(&manifest)?;
            validate_release_manifest(&release, &base)?;
            println!("validated {}", manifest.display());
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageLock {
    schema_version: u32,
    generated_at: String,
    images: LockedImages,
    artifacts: LockedArtifacts,
    toolchains: LockedToolchains,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedImages {
    node: LockedImage,
    rust: LockedImage,
    debian: LockedImage,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedImage {
    repository: String,
    tag: String,
    index_digest: String,
    platforms: BTreeMap<String, LockedImagePlatform>,
}

impl LockedImage {
    fn reference(&self) -> String {
        format!("{}:{}@{}", self.repository, self.tag, self.index_digest)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedImagePlatform {
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedArtifacts {
    uv: LockedUv,
    just: LockedUv,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedUv {
    kind: String,
    version: String,
    platforms: BTreeMap<String, LockedUvPlatform>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedUvPlatform {
    target: String,
    url: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedToolchains {
    rust: LockedRustToolchain,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedRustToolchain {
    version: String,
    profile: String,
    components: Vec<String>,
}

fn load_and_validate_lock(path: &Path) -> Result<ImageLock> {
    let contents = read_to_string(path)?;
    let lock: ImageLock = toml::from_str(&contents).map_err(|source| Error::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    validate_lock(&lock)?;
    Ok(lock)
}

fn validate_lock(lock: &ImageLock) -> Result<()> {
    ensure(
        lock.schema_version == 1,
        "image lock schema_version must be 1",
    )?;
    ensure(
        lock.generated_at.ends_with('Z') && lock.generated_at.contains('T'),
        "image lock generated_at must be a UTC RFC 3339 timestamp",
    )?;
    validate_image("node", &lock.images.node)?;
    validate_image("rust", &lock.images.rust)?;
    validate_image("debian", &lock.images.debian)?;

    let uv = &lock.artifacts.uv;
    ensure(uv.kind == "archive", "uv artifact kind must be archive")?;
    validate_version("uv", &uv.version)?;
    validate_platform_keys("uv artifact", &uv.platforms)?;
    for (platform, artifact) in &uv.platforms {
        validate_https_url(&format!("uv {platform}"), &artifact.url)?;
        validate_sha256(&format!("uv {platform}"), &artifact.sha256, false)?;
        ensure(
            artifact.url.contains(&uv.version) && artifact.url.contains(&artifact.target),
            format!("uv {platform} URL must contain version and target"),
        )?;
    }

    let just = &lock.artifacts.just;
    ensure(just.kind == "archive", "just artifact kind must be archive")?;
    validate_version("just", &just.version)?;
    validate_platform_keys("just artifact", &just.platforms)?;
    for (platform, artifact) in &just.platforms {
        validate_https_url(&format!("just {platform}"), &artifact.url)?;
        validate_sha256(&format!("just {platform}"), &artifact.sha256, false)?;
        ensure(
            artifact.url.contains(&just.version) && artifact.url.contains(&artifact.target),
            format!("just {platform} URL must contain version and target"),
        )?;
    }

    let toolchain = &lock.toolchains.rust;
    validate_version("Rust toolchain", &toolchain.version)?;
    ensure(
        matches!(
            toolchain.profile.as_str(),
            "minimal" | "default" | "complete"
        ),
        "Rust profile must be minimal, default, or complete",
    )?;
    let components: BTreeSet<&str> = toolchain.components.iter().map(String::as_str).collect();
    ensure(
        components.len() == toolchain.components.len(),
        "Rust toolchain components must be unique",
    )?;
    for required in ["clippy", "rustfmt", "rust-src", "rust-analyzer"] {
        ensure(
            components.contains(required),
            format!("Rust toolchain is missing {required}"),
        )?;
    }
    Ok(())
}

fn validate_image(name: &str, image: &LockedImage) -> Result<()> {
    ensure(
        !image.repository.contains("://") && image.repository.contains('/'),
        format!("{name} image repository must be a fully qualified OCI repository"),
    )?;
    ensure(
        !image.tag.is_empty() && image.tag != "latest",
        format!("{name} image must use an immutable versioned tag"),
    )?;
    validate_sha256(&format!("{name} image index"), &image.index_digest, true)?;
    validate_platform_keys(&format!("{name} image"), &image.platforms)?;
    for (platform, entry) in &image.platforms {
        validate_sha256(&format!("{name} image {platform}"), &entry.digest, true)?;
    }
    Ok(())
}

fn validate_platform_keys<T>(label: &str, platforms: &BTreeMap<String, T>) -> Result<()> {
    for platform in REQUIRED_PLATFORMS {
        ensure(
            platforms.contains_key(platform),
            format!("{label} is missing {platform}"),
        )?;
    }
    Ok(())
}

fn validate_version(label: &str, version: &str) -> Result<()> {
    ensure(
        !version.is_empty()
            && version != "latest"
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')),
        format!("{label} version is not an exact version: {version}"),
    )
}

fn validate_https_url(label: &str, url: &str) -> Result<()> {
    ensure(
        url.starts_with("https://") && !url.contains(char::is_whitespace),
        format!("{label} URL must be HTTPS without whitespace"),
    )
}

fn validate_sha256(label: &str, value: &str, prefixed: bool) -> Result<()> {
    let digest = if prefixed {
        value
            .strip_prefix("sha256:")
            .ok_or_else(|| Error::Validation(format!("{label} must start with sha256:")))?
    } else {
        value
    };
    ensure(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        format!("{label} must contain exactly 64 hexadecimal digits"),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentManifest {
    schema_version: u32,
    name: String,
    description: Option<String>,
    extends: Option<String>,
    image: Option<String>,
    build: Option<BuildConfig>,
    workdir: Option<PathBuf>,
    user: Option<String>,
    #[serde(default)]
    markers: Vec<PathBuf>,
    #[serde(default)]
    prepare: Vec<CommandSpec>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    mounts: Vec<Mount>,
    #[serde(default)]
    caches: Vec<Cache>,
    #[serde(default)]
    ports: Vec<Port>,
    #[serde(default)]
    host_services: Vec<HostService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildConfig {
    context: PathBuf,
    dockerfile: PathBuf,
    target: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpec {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mount {
    id: String,
    source: Option<PathBuf>,
    target: Option<PathBuf>,
    kind: Option<MountKind>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    remove: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MountKind {
    Bind,
    Volume,
    Tmpfs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    id: String,
    target: Option<PathBuf>,
    scope: Option<CacheScope>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    remove: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CacheScope {
    Shared,
    Project,
    Environment,
    Run,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Port {
    id: String,
    container: Option<u16>,
    host: Option<u16>,
    host_ip: Option<IpAddr>,
    protocol: Option<Protocol>,
    #[serde(default)]
    remove: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostService {
    id: String,
    host: Option<String>,
    port: Option<u16>,
    container_host: Option<String>,
    container_port: Option<u16>,
    #[serde(default)]
    allow_private: bool,
    #[serde(default)]
    remove: bool,
}

fn validate_manifests(root: &Path, lock: &ImageLock) -> Result<()> {
    let directory = root.join("assets/environments");
    let mut manifests = BTreeMap::new();
    for path in toml_files(&directory)? {
        let contents = read_to_string(&path)?;
        let manifest: EnvironmentManifest =
            toml::from_str(&contents).map_err(|source| Error::Toml {
                path: path.clone(),
                source,
            })?;
        let file_name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                Error::Validation(format!("invalid manifest filename: {}", path.display()))
            })?
            .to_owned();
        ensure(
            file_name == manifest.name,
            format!(
                "manifest {} declares name {:?}",
                path.display(),
                manifest.name
            ),
        )?;
        validate_manifest(&path, &manifest, lock)?;
        ensure(
            manifests
                .insert(manifest.name.clone(), (path, manifest))
                .is_none(),
            format!("duplicate environment name {file_name}"),
        )?;
    }
    for required in REQUIRED_ENVIRONMENTS {
        ensure(
            manifests.contains_key(required),
            format!("missing built-in environment {required}"),
        )?;
    }
    for (name, (_, manifest)) in &manifests {
        if let Some(parent) = &manifest.extends {
            ensure(
                manifests.contains_key(parent),
                format!("environment {name} extends unknown environment {parent}"),
            )?;
        }
    }
    validate_inheritance(&manifests)?;
    Ok(())
}

fn validate_manifest(path: &Path, manifest: &EnvironmentManifest, lock: &ImageLock) -> Result<()> {
    ensure(
        manifest.schema_version == 1,
        format!("{} schema_version must be 1", path.display()),
    )?;
    validate_identifier("environment", &manifest.name)?;
    if let Some(description) = &manifest.description {
        ensure(
            !description.trim().is_empty(),
            format!("{} has an empty description", manifest.name),
        )?;
    }
    if let Some(parent) = &manifest.extends {
        validate_identifier("parent environment", parent)?;
        ensure(
            parent != &manifest.name,
            format!("{} cannot extend itself", manifest.name),
        )?;
    }
    ensure(
        !(manifest.image.is_some() && manifest.build.is_some()),
        format!("{} cannot define both image and build", manifest.name),
    )?;
    if let Some(image) = &manifest.image {
        ensure(
            image.contains("@sha256:") && !image.ends_with(":latest"),
            format!("{} image must include an immutable digest", manifest.name),
        )?;
    }
    if let Some(workdir) = &manifest.workdir {
        ensure_absolute_path("workdir", workdir, &manifest.name)?;
    }
    if let Some(user) = &manifest.user {
        ensure(
            !user.trim().is_empty() && !user.contains(char::is_whitespace),
            format!("{} has an invalid user", manifest.name),
        )?;
    }
    validate_unique_paths("marker", &manifest.markers, &manifest.name)?;
    for marker in &manifest.markers {
        ensure(
            !marker.is_absolute() && safe_relative_path(marker),
            format!("{} has unsafe marker {}", manifest.name, marker.display()),
        )?;
    }
    for command in &manifest.prepare {
        ensure(
            !command.program.trim().is_empty()
                && !command.program.contains('\0')
                && command.args.iter().all(|arg| !arg.contains('\0')),
            format!("{} has an invalid preparation command", manifest.name),
        )?;
    }
    for (key, value) in &manifest.env {
        ensure(
            valid_environment_key(key) && !value.contains('\0'),
            format!("{} has an invalid environment entry {key}", manifest.name),
        )?;
    }
    let mut hosts = BTreeSet::new();
    for host in &manifest.allow_hosts {
        ensure(
            valid_allowed_host(host),
            format!("{} has an invalid allowed host {host:?}", manifest.name),
        )?;
        ensure(
            hosts.insert(host),
            format!("{} repeats allowed host {host}", manifest.name),
        )?;
    }
    validate_resources(manifest)?;
    if let Some(build) = &manifest.build {
        validate_build(path, &manifest.name, build, lock)?;
    }
    Ok(())
}

fn validate_build(
    manifest_path: &Path,
    environment: &str,
    build: &BuildConfig,
    lock: &ImageLock,
) -> Result<()> {
    let manifest_directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let context = manifest_directory.join(&build.context);
    ensure(
        context.is_dir(),
        format!(
            "{environment} build context does not exist: {}",
            context.display()
        ),
    )?;
    let dockerfile = context.join(&build.dockerfile);
    ensure(
        dockerfile.is_file(),
        format!(
            "{environment} Dockerfile does not exist: {}",
            dockerfile.display()
        ),
    )?;
    if let Some(target) = &build.target {
        validate_identifier("Docker target", target)?;
    }
    let expected = docker_build_args(lock);
    for key in ["NODE_IMAGE", "RUST_IMAGE"] {
        ensure(
            build.args.get(key) == expected.get(key),
            format!("{environment} build argument {key} differs from images.lock.toml"),
        )?;
    }
    if environment == "uv" {
        for key in [
            "UV_VERSION",
            "UV_SHA256_AMD64",
            "UV_SHA256_ARM64",
            "JUST_VERSION",
            "JUST_SHA256_AMD64",
            "JUST_SHA256_ARM64",
        ] {
            ensure(
                build.args.get(key) == expected.get(key),
                format!("uv build argument {key} differs from images.lock.toml"),
            )?;
        }
    }
    Ok(())
}

fn validate_resources(manifest: &EnvironmentManifest) -> Result<()> {
    let name = &manifest.name;
    let mut ids = BTreeSet::new();
    for mount in &manifest.mounts {
        validate_resource_id("mount", &mount.id, &mut ids, name)?;
        if !mount.remove {
            let target = mount.target.as_ref().ok_or_else(|| {
                Error::Validation(format!("{name} mount {} is missing target", mount.id))
            })?;
            ensure_absolute_path("mount target", target, name)?;
            let kind = mount.kind.as_ref().ok_or_else(|| {
                Error::Validation(format!("{name} mount {} is missing kind", mount.id))
            })?;
            if matches!(kind, MountKind::Bind) {
                ensure(
                    mount.source.is_some(),
                    format!("{name} bind mount {} is missing source", mount.id),
                )?;
            }
            let _ = mount.read_only;
        }
    }
    ids.clear();
    for cache in &manifest.caches {
        validate_resource_id("cache", &cache.id, &mut ids, name)?;
        if !cache.remove {
            let target = cache.target.as_ref().ok_or_else(|| {
                Error::Validation(format!("{name} cache {} is missing target", cache.id))
            })?;
            ensure_absolute_path("cache target", target, name)?;
            ensure(
                cache.scope.is_some(),
                format!("{name} cache {} is missing scope", cache.id),
            )?;
            let _ = cache.read_only;
        }
    }
    ids.clear();
    for port in &manifest.ports {
        validate_resource_id("port", &port.id, &mut ids, name)?;
        if !port.remove {
            ensure(
                port.container.is_some_and(|value| value != 0),
                format!("{name} port {} has no container port", port.id),
            )?;
            ensure(
                port.host.is_none_or(|value| value != 0),
                format!("{name} port {} has invalid host port", port.id),
            )?;
            let _ = (&port.host_ip, &port.protocol);
        }
    }
    ids.clear();
    for service in &manifest.host_services {
        validate_resource_id("host service", &service.id, &mut ids, name)?;
        if !service.remove {
            ensure(
                service.host.as_ref().is_some_and(|host| !host.is_empty())
                    && service.port.is_some_and(|port| port != 0)
                    && service
                        .container_host
                        .as_ref()
                        .is_some_and(|host| !host.is_empty())
                    && service.container_port.is_some_and(|port| port != 0),
                format!("{name} host service {} is incomplete", service.id),
            )?;
            let _ = service.allow_private;
        }
    }
    Ok(())
}

fn validate_resource_id<'a>(
    kind: &str,
    id: &'a str,
    ids: &mut BTreeSet<&'a str>,
    environment: &str,
) -> Result<()> {
    validate_identifier(kind, id)?;
    ensure(
        ids.insert(id),
        format!("{environment} repeats {kind} id {id}"),
    )
}

fn validate_inheritance(
    manifests: &BTreeMap<String, (PathBuf, EnvironmentManifest)>,
) -> Result<()> {
    for name in manifests.keys() {
        let mut seen = BTreeSet::new();
        let mut current = name.as_str();
        loop {
            ensure(
                seen.insert(current),
                format!("environment inheritance cycle involving {current}"),
            )?;
            let Some(parent) = manifests
                .get(current)
                .and_then(|(_, manifest)| manifest.extends.as_deref())
            else {
                break;
            };
            current = parent;
        }
    }
    Ok(())
}

fn validate_repository(root: &Path) -> Result<()> {
    let lock_path = root.join("assets/images.lock.toml");
    let lock = load_and_validate_lock(&lock_path)?;
    validate_manifests(root, &lock)?;
    validate_docker_pins(root, &lock)
}

fn validate_docker_pins(root: &Path, lock: &ImageLock) -> Result<()> {
    let environment = read_to_string(&root.join("images/environment/Dockerfile"))?;
    let sidecar = read_to_string(&root.join("images/sidecar/Dockerfile"))?;
    let expected = docker_build_args(lock);
    for key in [
        "NODE_IMAGE",
        "RUST_IMAGE",
        "UV_VERSION",
        "UV_SHA256_AMD64",
        "UV_SHA256_ARM64",
        "JUST_VERSION",
        "JUST_SHA256_AMD64",
        "JUST_SHA256_ARM64",
    ] {
        let value = expected
            .get(key)
            .ok_or_else(|| Error::Validation(format!("internal missing build argument {key}")))?;
        ensure(
            environment.contains(&format!("{key}={value}")),
            format!("environment Dockerfile does not pin {key} from images.lock.toml"),
        )?;
    }
    for key in ["RUST_IMAGE", "DEBIAN_IMAGE"] {
        let value = expected
            .get(key)
            .ok_or_else(|| Error::Validation(format!("internal missing build argument {key}")))?;
        ensure(
            sidecar.contains(&format!("{key}={value}")),
            format!("sidecar Dockerfile does not pin {key} from images.lock.toml"),
        )?;
    }
    ensure(
        !environment.contains(":latest") && !sidecar.contains(":latest"),
        "Dockerfiles may not use latest tags",
    )
}

fn docker_build_args(lock: &ImageLock) -> BTreeMap<String, String> {
    let mut args = BTreeMap::new();
    args.insert("NODE_IMAGE".to_owned(), lock.images.node.reference());
    args.insert("RUST_IMAGE".to_owned(), lock.images.rust.reference());
    args.insert("DEBIAN_IMAGE".to_owned(), lock.images.debian.reference());
    args.insert("UV_VERSION".to_owned(), lock.artifacts.uv.version.clone());
    args.insert(
        "UV_SHA256_AMD64".to_owned(),
        lock.artifacts.uv.platforms["linux/amd64"].sha256.clone(),
    );
    args.insert(
        "UV_SHA256_ARM64".to_owned(),
        lock.artifacts.uv.platforms["linux/arm64"].sha256.clone(),
    );
    args.insert(
        "JUST_VERSION".to_owned(),
        lock.artifacts.just.version.clone(),
    );
    args.insert(
        "JUST_SHA256_AMD64".to_owned(),
        lock.artifacts.just.platforms["linux/amd64"].sha256.clone(),
    );
    args.insert(
        "JUST_SHA256_ARM64".to_owned(),
        lock.artifacts.just.platforms["linux/arm64"].sha256.clone(),
    );
    args
}

fn print_build_args(lock: &ImageLock, format: OutputFormat) -> Result<()> {
    let args = docker_build_args(lock);
    match format {
        OutputFormat::Lines => {
            for (key, value) in args {
                println!("{key}={value}");
            }
        }
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&args)?),
    }
    Ok(())
}

fn image_tag(
    root: &Path,
    environment: &str,
    architecture: Architecture,
    version: &str,
) -> Result<String> {
    validate_version("release", version)?;
    validate_identifier("environment", environment)?;
    let manifest_directory = root.join("assets/environments");
    let manifest_path = manifest_directory.join(format!("{environment}.toml"));
    ensure(
        manifest_path.is_file(),
        format!("unknown environment {environment}"),
    )?;
    let lock = load_and_validate_lock(&root.join("assets/images.lock.toml"))?;
    validate_manifests(root, &lock)?;

    let mut files = vec![root.join("assets/images.lock.toml")];
    let mut current = environment.to_owned();
    let mut seen = BTreeSet::new();
    while seen.insert(current.clone()) {
        let path = manifest_directory.join(format!("{current}.toml"));
        let contents = read_to_string(&path)?;
        let manifest: EnvironmentManifest =
            toml::from_str(&contents).map_err(|source| Error::Toml {
                path: path.clone(),
                source,
            })?;
        files.push(path);
        let Some(parent) = manifest.extends else {
            break;
        };
        current = parent;
    }
    files.extend(files_below(&root.join("images"))?);
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"codex-start-image-v1\0");
    hasher.update(environment.as_bytes());
    hasher.update([0]);
    hasher.update(architecture.as_str().as_bytes());
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let contents = read_bytes(&path)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update(contents);
    }
    let digest = hex_encode(hasher.finalize());
    Ok(format!(
        "codex-start-{environment}:{version}-{}-{}",
        architecture.as_str(),
        &digest[..16]
    ))
}

#[derive(Debug)]
struct ReleaseArchiveEntry {
    source: PathBuf,
    relative_name: String,
    kind: ReleaseArchiveEntryKind,
}

#[derive(Debug)]
enum ReleaseArchiveEntryKind {
    Directory,
    File,
    Symlink(String),
}

#[derive(Clone, Copy)]
struct ReleaseTarEntry<'a> {
    name: &'a str,
    link_name: Option<&'a str>,
    entry_type: TarEntryType,
    size: u64,
    mode: u32,
}

impl<'a> ReleaseTarEntry<'a> {
    fn directory(name: &'a str) -> Self {
        Self {
            name,
            link_name: None,
            entry_type: TarEntryType::Directory,
            size: 0,
            mode: 0o755,
        }
    }

    fn regular(name: &'a str, size: u64, executable: bool) -> Self {
        Self {
            name,
            link_name: None,
            entry_type: TarEntryType::Regular,
            size,
            mode: if executable { 0o755 } else { 0o644 },
        }
    }

    fn symlink(name: &'a str, link_name: &'a str) -> Self {
        Self {
            name,
            link_name: Some(link_name),
            entry_type: TarEntryType::Symlink,
            size: 0,
            mode: 0o777,
        }
    }
}

fn write_release_archive(
    directory: &Path,
    output: &Path,
    prefix: &Path,
    source_date_epoch: u32,
    executable_paths: &[PathBuf],
) -> Result<()> {
    let root_metadata = fs::symlink_metadata(directory).map_err(|source| Error::Read {
        path: directory.to_path_buf(),
        source,
    })?;
    ensure(
        root_metadata.file_type().is_dir(),
        format!(
            "release archive source is not a directory: {}",
            directory.display()
        ),
    )?;
    let format = release_archive_format(output)?;
    ensure(
        !output.try_exists().map_err(|source| Error::Read {
            path: output.to_path_buf(),
            source,
        })?,
        format!("release archive already exists: {}", output.display()),
    )?;

    let prefix_name = archive_relative_name(prefix, "archive prefix")?;
    let entries = collect_release_archive_entries(directory)?;
    let executables = validate_archive_executables(executable_paths, &entries)?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure(
        parent.is_dir(),
        format!(
            "release archive destination directory does not exist: {}",
            parent.display()
        ),
    )?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })?;
    match format {
        ReleaseArchiveFormat::TarGzip => write_release_tar_gzip(
            temporary.as_file_mut(),
            output,
            &prefix_name,
            source_date_epoch,
            &entries,
            &executables,
        )?,
        ReleaseArchiveFormat::Zip => write_release_zip(
            temporary.as_file_mut(),
            output,
            &prefix_name,
            source_date_epoch,
            &entries,
            &executables,
        )?,
    }
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| Error::Write {
            path: output.to_path_buf(),
            source,
        })?;
    temporary
        .persist_noclobber(output)
        .map_err(|source| Error::Persist {
            path: output.to_path_buf(),
            source,
        })?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleaseArchiveFormat {
    TarGzip,
    Zip,
}

fn release_archive_format(output: &Path) -> Result<ReleaseArchiveFormat> {
    let name = output.file_name().and_then(OsStr::to_str).ok_or_else(|| {
        Error::Validation("release archive output must have a UTF-8 filename".to_owned())
    })?;
    if name.len() > ".tar.gz".len() && name.ends_with(".tar.gz") {
        Ok(ReleaseArchiveFormat::TarGzip)
    } else if name.len() > ".zip".len()
        && Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        Ok(ReleaseArchiveFormat::Zip)
    } else {
        Err(Error::Validation(
            "release archive output must end in .tar.gz or .zip".to_owned(),
        ))
    }
}

fn write_release_zip(
    destination: &mut fs::File,
    output: &Path,
    prefix: &str,
    source_date_epoch: u32,
    entries: &[ReleaseArchiveEntry],
    executables: &BTreeSet<String>,
) -> Result<()> {
    let modified = zip_datetime(source_date_epoch)?;
    let directory_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(modified)
        .unix_permissions(0o755)
        .system(zip::System::Unix);
    let mut archive = zip::ZipWriter::new(destination);
    archive.add_directory(format!("{prefix}/"), directory_options)?;
    for entry in entries {
        let archive_name = format!("{prefix}/{}", entry.relative_name);
        match &entry.kind {
            ReleaseArchiveEntryKind::Directory => {
                archive.add_directory(format!("{archive_name}/"), directory_options)?;
            }
            ReleaseArchiveEntryKind::File => {
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .compression_level(Some(9))
                    .last_modified_time(modified)
                    .unix_permissions(if executables.contains(&entry.relative_name) {
                        0o755
                    } else {
                        0o644
                    })
                    .system(zip::System::Unix);
                archive.start_file(&archive_name, options)?;
                let mut file = fs::File::open(&entry.source).map_err(|source| Error::Read {
                    path: entry.source.clone(),
                    source,
                })?;
                io::copy(&mut file, &mut archive).map_err(|source| Error::Write {
                    path: output.to_path_buf(),
                    source,
                })?;
            }
            ReleaseArchiveEntryKind::Symlink(_) => {
                return Err(Error::Validation(format!(
                    "ZIP release archives cannot contain symlinks: {}",
                    entry.source.display()
                )));
            }
        }
    }
    archive.finish()?;
    Ok(())
}

fn zip_datetime(source_date_epoch: u32) -> Result<zip::DateTime> {
    let seconds = i64::from(source_date_epoch);
    let days = seconds / 86_400;
    let seconds_in_day = seconds % 86_400;
    let (year, month, day) = civil_date_from_unix_days(days);
    let hour = u8::try_from(seconds_in_day / 3_600)
        .expect("seconds in a UTC day always produce a valid hour");
    let minute = u8::try_from((seconds_in_day % 3_600) / 60)
        .expect("seconds in a UTC hour always produce a valid minute");
    let second = u8::try_from(seconds_in_day % 60)
        .expect("seconds in a UTC minute always produce a valid second");
    let year = u16::try_from(year).map_err(|_| {
        Error::Validation(format!(
            "SOURCE_DATE_EPOCH {source_date_epoch} is outside the ZIP timestamp range"
        ))
    })?;
    zip::DateTime::from_date_and_time(year, month, day, hour, minute, second).map_err(|_| {
        Error::Validation(format!(
            "SOURCE_DATE_EPOCH {source_date_epoch} is outside the ZIP timestamp range"
        ))
    })
}

fn civil_date_from_unix_days(days: i64) -> (i64, u8, u8) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u8::try_from(month).expect("civil-date month is within 1..=12"),
        u8::try_from(day).expect("civil-date day is within 1..=31"),
    )
}

fn write_release_tar_gzip(
    destination: &mut fs::File,
    output: &Path,
    prefix: &str,
    source_date_epoch: u32,
    entries: &[ReleaseArchiveEntry],
    executables: &BTreeSet<String>,
) -> Result<()> {
    let encoder = GzBuilder::new()
        .mtime(source_date_epoch)
        .operating_system(255)
        .write(destination, Compression::best());
    let mut archive = TarBuilder::new(encoder);
    archive.mode(HeaderMode::Deterministic);
    append_release_tar_entry(
        &mut archive,
        output,
        ReleaseTarEntry::directory(prefix),
        source_date_epoch,
        io::empty(),
    )?;
    for entry in entries {
        let archive_name = format!("{prefix}/{}", entry.relative_name);
        match &entry.kind {
            ReleaseArchiveEntryKind::Directory => append_release_tar_entry(
                &mut archive,
                output,
                ReleaseTarEntry::directory(&archive_name),
                source_date_epoch,
                io::empty(),
            )?,
            ReleaseArchiveEntryKind::File => {
                let file = fs::File::open(&entry.source).map_err(|source| Error::Read {
                    path: entry.source.clone(),
                    source,
                })?;
                let size = file
                    .metadata()
                    .map_err(|source| Error::Read {
                        path: entry.source.clone(),
                        source,
                    })?
                    .len();
                append_release_tar_entry(
                    &mut archive,
                    output,
                    ReleaseTarEntry::regular(
                        &archive_name,
                        size,
                        executables.contains(&entry.relative_name),
                    ),
                    source_date_epoch,
                    file,
                )?;
            }
            ReleaseArchiveEntryKind::Symlink(target) => append_release_tar_entry(
                &mut archive,
                output,
                ReleaseTarEntry::symlink(&archive_name, target),
                source_date_epoch,
                io::empty(),
            )?,
        }
    }
    archive.finish().map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })?;
    let encoder = archive.into_inner().map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })?;
    encoder.finish().map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn append_release_tar_entry<R: Read>(
    archive: &mut TarBuilder<flate2::write::GzEncoder<&mut fs::File>>,
    output: &Path,
    entry: ReleaseTarEntry<'_>,
    source_date_epoch: u32,
    contents: R,
) -> Result<()> {
    let mut header = TarHeader::new_gnu();
    header.set_entry_type(entry.entry_type);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(u64::from(source_date_epoch));
    header.set_mode(entry.mode);
    header.set_size(entry.size);
    let result = if let Some(link_name) = entry.link_name {
        archive.append_link(&mut header, entry.name, link_name)
    } else {
        archive.append_data(&mut header, entry.name, contents)
    };
    result.map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })
}

fn collect_release_archive_entries(directory: &Path) -> Result<Vec<ReleaseArchiveEntry>> {
    let mut pending = vec![directory.to_path_buf()];
    let mut entries = Vec::new();
    while let Some(current) = pending.pop() {
        let children = fs::read_dir(&current).map_err(|source| Error::Read {
            path: current.clone(),
            source,
        })?;
        for child in children {
            let child = child.map_err(|source| Error::Read {
                path: current.clone(),
                source,
            })?;
            let path = child.path();
            let relative = path.strip_prefix(directory).map_err(|source| {
                Error::Validation(format!(
                    "failed to make {} relative to {}: {source}",
                    path.display(),
                    directory.display()
                ))
            })?;
            let relative_name = archive_relative_name(relative, "archive entry")?;
            let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let file_type = metadata.file_type();
            let kind = if file_type.is_dir() {
                pending.push(path.clone());
                ReleaseArchiveEntryKind::Directory
            } else if file_type.is_file() {
                ReleaseArchiveEntryKind::File
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path).map_err(|source| Error::Read {
                    path: path.clone(),
                    source,
                })?;
                ReleaseArchiveEntryKind::Symlink(safe_archive_symlink_target(relative, &target)?)
            } else {
                return Err(Error::Validation(format!(
                    "release archive contains unsupported file type: {}",
                    path.display()
                )));
            };
            entries.push(ReleaseArchiveEntry {
                source: path,
                relative_name,
                kind,
            });
        }
    }
    entries.sort_by(|left, right| left.relative_name.cmp(&right.relative_name));
    Ok(entries)
}

fn validate_archive_executables(
    executable_paths: &[PathBuf],
    entries: &[ReleaseArchiveEntry],
) -> Result<BTreeSet<String>> {
    let regular_files: BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| {
            matches!(entry.kind, ReleaseArchiveEntryKind::File)
                .then_some(entry.relative_name.as_str())
        })
        .collect();
    let mut executables = BTreeSet::new();
    for path in executable_paths {
        let name = archive_relative_name(path, "executable path")?;
        ensure(
            regular_files.contains(name.as_str()),
            format!("archive executable is not a regular file: {name}"),
        )?;
        ensure(
            executables.insert(name.clone()),
            format!("archive executable was specified more than once: {name}"),
        )?;
    }
    Ok(executables)
}

fn archive_relative_name(path: &Path, label: &str) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(Error::Validation(format!(
                "{label} must be a normalized relative path: {}",
                path.display()
            )));
        };
        parts.push(archive_component(part, label)?);
    }
    ensure(!parts.is_empty(), format!("{label} must not be empty"))?;
    Ok(parts.join("/"))
}

fn archive_component(part: &OsStr, label: &str) -> Result<String> {
    let part = part.to_str().ok_or_else(|| {
        Error::Validation(format!(
            "{label} must contain only valid UTF-8 path components"
        ))
    })?;
    ensure(
        !part.is_empty() && !part.contains(['/', '\\', ':']) && !part.chars().any(char::is_control),
        format!("{label} contains an unsafe path component: {part:?}"),
    )?;
    Ok(part.to_owned())
}

fn safe_archive_symlink_target(relative: &Path, target: &Path) -> Result<String> {
    ensure(
        !target.as_os_str().is_empty() && target.is_relative(),
        format!(
            "archive symlink {} must have a non-empty relative target",
            relative.display()
        ),
    )?;
    let mut depth = relative
        .parent()
        .map_or(0, |parent| parent.components().count());
    let mut parts = Vec::new();
    for component in target.components() {
        match component {
            Component::Normal(part) => {
                parts.push(archive_component(part, "symlink target")?);
                depth += 1;
            }
            Component::CurDir => parts.push(".".to_owned()),
            Component::ParentDir => {
                ensure(
                    depth > 0,
                    format!(
                        "archive symlink {} escapes the archive root",
                        relative.display()
                    ),
                )?;
                depth -= 1;
                parts.push("..".to_owned());
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(Error::Validation(format!(
                    "archive symlink {} has an absolute target",
                    relative.display()
                )));
            }
        }
    }
    ensure(
        !parts.is_empty(),
        format!("archive symlink {} has an empty target", relative.display()),
    )?;
    Ok(parts.join("/"))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_encode(hasher.finalize()))
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn write_release_checksums(directory: &Path, output: &Path) -> Result<()> {
    ensure(
        directory.is_dir(),
        format!("release directory does not exist: {}", directory.display()),
    )?;
    let mut files = files_below(directory)?;
    files.retain(|path| path != output);
    ensure(!files.is_empty(), "release directory contains no files")?;
    let mut contents = String::new();
    for path in files {
        let relative = path.strip_prefix(directory).map_err(|error| {
            Error::Validation(format!(
                "failed to make {} relative: {error}",
                path.display()
            ))
        })?;
        writeln!(
            contents,
            "{}  {}",
            sha256_file(&path)?,
            slash_path(relative)
        )
        .expect("writing to a String cannot fail");
    }
    fs::write(output, contents).map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })
}

fn verify_checksums(manifest: &Path, base: &Path) -> Result<()> {
    let contents = read_to_string(manifest)?;
    let mut count = 0_u64;
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (expected, relative) = line.split_once("  ").ok_or_else(|| {
            Error::Validation(format!(
                "{}:{} is not in SHA256SUMS format",
                manifest.display(),
                index + 1
            ))
        })?;
        validate_sha256("release checksum", expected, false)?;
        let relative = Path::new(relative);
        ensure(
            safe_relative_path(relative),
            format!("unsafe checksum path {}", relative.display()),
        )?;
        let path = base.join(relative);
        let actual = sha256_file(&path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(Error::ChecksumMismatch {
                path,
                expected: expected.to_owned(),
                actual,
            });
        }
        count += 1;
    }
    ensure(count > 0, "checksum manifest contains no entries")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseManifest {
    schema_version: u32,
    version: String,
    tag: String,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReleaseArtifactKind {
    Archive,
    Deb,
    Rpm,
    Apk,
    Installer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReleaseOperatingSystem {
    Linux,
    Macos,
    Posix,
    Windows,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReleaseArchitecture {
    X86_64,
    Aarch64,
    Any,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifact {
    kind: ReleaseArtifactKind,
    os: ReleaseOperatingSystem,
    arch: ReleaseArchitecture,
    libc: Option<String>,
    filename: String,
    size: u64,
    sha256: String,
    bundle: String,
    sbom: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReleaseArtifactIdentity {
    kind: ReleaseArtifactKind,
    os: ReleaseOperatingSystem,
    arch: ReleaseArchitecture,
    libc: Option<&'static str>,
    filename: String,
}

impl ReleaseArtifactIdentity {
    fn archive(
        version: &str,
        target: &str,
        os: ReleaseOperatingSystem,
        arch: ReleaseArchitecture,
        libc: Option<&'static str>,
        extension: &str,
    ) -> Self {
        Self {
            kind: ReleaseArtifactKind::Archive,
            os,
            arch,
            libc,
            filename: format!("codex-start-{version}-{target}.{extension}"),
        }
    }

    fn package(
        version: &str,
        target: &str,
        kind: ReleaseArtifactKind,
        arch: ReleaseArchitecture,
        libc: &'static str,
        extension: &str,
    ) -> Self {
        Self {
            kind,
            os: ReleaseOperatingSystem::Linux,
            arch,
            libc: Some(libc),
            filename: format!("codex-start-{version}-{target}.{extension}"),
        }
    }

    fn installer(filename: &str, os: ReleaseOperatingSystem, arch: ReleaseArchitecture) -> Self {
        Self {
            kind: ReleaseArtifactKind::Installer,
            os,
            arch,
            libc: None,
            filename: filename.to_owned(),
        }
    }
}

fn expected_release_artifacts(version: &str) -> Vec<ReleaseArtifactIdentity> {
    let mut artifacts = expected_release_archives(version);
    artifacts.extend(expected_release_packages(version));
    artifacts.extend([
        ReleaseArtifactIdentity::installer(
            "install.sh",
            ReleaseOperatingSystem::Posix,
            ReleaseArchitecture::Any,
        ),
        ReleaseArtifactIdentity::installer(
            "install.ps1",
            ReleaseOperatingSystem::Windows,
            ReleaseArchitecture::Any,
        ),
    ]);
    artifacts.sort_by(|left, right| left.filename.cmp(&right.filename));
    artifacts
}

fn expected_release_archives(version: &str) -> Vec<ReleaseArtifactIdentity> {
    use ReleaseArchitecture::{Aarch64, X86_64};
    use ReleaseOperatingSystem::{Linux, Macos, Windows};

    vec![
        ReleaseArtifactIdentity::archive(
            version,
            "x86_64-unknown-linux-gnu",
            Linux,
            X86_64,
            Some("gnu"),
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "aarch64-unknown-linux-gnu",
            Linux,
            Aarch64,
            Some("gnu"),
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "x86_64-unknown-linux-musl",
            Linux,
            X86_64,
            Some("musl"),
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "aarch64-unknown-linux-musl",
            Linux,
            Aarch64,
            Some("musl"),
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "x86_64-apple-darwin",
            Macos,
            X86_64,
            None,
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "aarch64-apple-darwin",
            Macos,
            Aarch64,
            None,
            "tar.gz",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "x86_64-pc-windows-msvc",
            Windows,
            X86_64,
            None,
            "zip",
        ),
        ReleaseArtifactIdentity::archive(
            version,
            "aarch64-pc-windows-msvc",
            Windows,
            Aarch64,
            None,
            "zip",
        ),
    ]
}

fn expected_release_packages(version: &str) -> Vec<ReleaseArtifactIdentity> {
    use ReleaseArchitecture::{Aarch64, X86_64};
    use ReleaseArtifactKind::{Apk, Deb, Rpm};

    vec![
        ReleaseArtifactIdentity::package(
            version,
            "x86_64-unknown-linux-gnu",
            Deb,
            X86_64,
            "gnu",
            "deb",
        ),
        ReleaseArtifactIdentity::package(
            version,
            "aarch64-unknown-linux-gnu",
            Deb,
            Aarch64,
            "gnu",
            "deb",
        ),
        ReleaseArtifactIdentity::package(
            version,
            "x86_64-unknown-linux-gnu",
            Rpm,
            X86_64,
            "gnu",
            "rpm",
        ),
        ReleaseArtifactIdentity::package(
            version,
            "aarch64-unknown-linux-gnu",
            Rpm,
            Aarch64,
            "gnu",
            "rpm",
        ),
        ReleaseArtifactIdentity::package(
            version,
            "x86_64-unknown-linux-musl",
            Apk,
            X86_64,
            "musl",
            "apk",
        ),
        ReleaseArtifactIdentity::package(
            version,
            "aarch64-unknown-linux-musl",
            Apk,
            Aarch64,
            "musl",
            "apk",
        ),
    ]
}

fn write_release_manifest(directory: &Path, output: &Path, version: &str) -> Result<()> {
    ensure(
        directory.is_dir(),
        format!("release directory does not exist: {}", directory.display()),
    )?;
    validate_semantic_version(version)?;
    ensure(
        !output.try_exists().map_err(|source| Error::Read {
            path: output.to_path_buf(),
            source,
        })?,
        format!("release manifest already exists: {}", output.display()),
    )?;

    let mut artifacts = Vec::new();
    for expected in expected_release_artifacts(version) {
        let path = directory.join(&expected.filename);
        let metadata = fs::metadata(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        ensure(
            metadata.is_file(),
            format!("release artifact is not a file: {}", path.display()),
        )?;
        artifacts.push(ReleaseArtifact {
            kind: expected.kind,
            os: expected.os,
            arch: expected.arch,
            libc: expected.libc.map(str::to_owned),
            filename: expected.filename.clone(),
            size: metadata.len(),
            sha256: sha256_file(&path)?,
            bundle: format!("{}.bundle", expected.filename),
            sbom: format!("{}.spdx.json", expected.filename),
        });
    }
    let manifest = ReleaseManifest {
        schema_version: 1,
        version: version.to_owned(),
        tag: format!("v{version}"),
        artifacts,
    };
    validate_release_manifest(&manifest, directory)?;
    let mut encoded = serde_json::to_vec_pretty(&manifest)?;
    encoded.push(b'\n');
    fs::write(output, encoded).map_err(|source| Error::Write {
        path: output.to_path_buf(),
        source,
    })
}

fn load_release_manifest(path: &Path) -> Result<ReleaseManifest> {
    let contents = read_to_string(path)?;
    serde_json::from_str(&contents).map_err(Error::Json)
}

fn validate_release_manifest(manifest: &ReleaseManifest, base: &Path) -> Result<()> {
    ensure(
        manifest.schema_version == 1,
        "release manifest schema_version must be 1",
    )?;
    validate_semantic_version(&manifest.version)?;
    ensure(
        manifest.tag == format!("v{}", manifest.version),
        "release manifest tag must be v followed by version",
    )?;
    let expected = expected_release_artifacts(&manifest.version);
    ensure(
        manifest.artifacts.len() == expected.len(),
        format!(
            "release manifest must contain exactly {} artifacts",
            expected.len()
        ),
    )?;

    let mut filenames = BTreeSet::new();
    for (artifact, expected) in manifest.artifacts.iter().zip(&expected) {
        ensure(
            artifact.kind == expected.kind
                && artifact.os == expected.os
                && artifact.arch == expected.arch
                && artifact.libc.as_deref() == expected.libc
                && artifact.filename == expected.filename,
            format!(
                "release artifact order or metadata differs from the required matrix at {}",
                artifact.filename
            ),
        )?;
        ensure(
            filenames.insert(&artifact.filename),
            format!("release manifest repeats {}", artifact.filename),
        )?;
        ensure(
            safe_release_filename(&artifact.filename),
            format!("unsafe release artifact filename {}", artifact.filename),
        )?;
        validate_sha256("release artifact", &artifact.sha256, false)?;
        ensure(
            artifact.bundle == format!("{}.bundle", artifact.filename),
            format!("invalid bundle name for {}", artifact.filename),
        )?;
        ensure(
            artifact.sbom == format!("{}.spdx.json", artifact.filename),
            format!("invalid SBOM name for {}", artifact.filename),
        )?;

        let path = base.join(&artifact.filename);
        let metadata = fs::metadata(&path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        ensure(
            metadata.is_file() && metadata.len() == artifact.size,
            format!("release artifact size differs for {}", artifact.filename),
        )?;
        let actual = sha256_file(&path)?;
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(Error::ChecksumMismatch {
                path,
                expected: artifact.sha256.clone(),
                actual,
            });
        }
        for related in [&artifact.bundle, &artifact.sbom] {
            ensure(
                safe_release_filename(related),
                format!("unsafe related release filename {related}"),
            )?;
            let related_path = base.join(related);
            let related_metadata = fs::metadata(&related_path).map_err(|source| Error::Read {
                path: related_path.clone(),
                source,
            })?;
            ensure(
                related_metadata.is_file() && related_metadata.len() > 0,
                format!("release metadata file is empty: {}", related_path.display()),
            )?;
        }
    }
    Ok(())
}

fn validate_semantic_version(version: &str) -> Result<()> {
    let parsed = semver::Version::parse(version).map_err(|error| {
        Error::Validation(format!(
            "release version is not valid SemVer: {version}: {error}"
        ))
    })?;
    ensure(
        parsed.to_string() == version,
        format!("release version must use canonical SemVer: {version}"),
    )
}

fn safe_release_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= 255
        && !filename.starts_with('.')
        && filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

fn read_to_string(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn toml_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = files_below(directory)?;
    files.retain(|path| path.extension() == Some(OsStr::new("toml")));
    Ok(files)
}

fn files_below(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![directory.to_path_buf()];
    let mut files = Vec::new();
    while let Some(current) = pending.pop() {
        let entries = fs::read_dir(&current).map_err(|source| Error::Read {
            path: current.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::Read {
                path: current.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| Error::Read {
                path: entry.path(),
                source,
            })?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    ensure(
        !value.is_empty()
            && value.len() <= 64
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            }),
        format!("invalid {kind} identifier {value:?}"),
    )
}

fn validate_unique_paths(kind: &str, paths: &[PathBuf], environment: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        ensure(
            seen.insert(path),
            format!("{environment} repeats {kind} {}", path.display()),
        )?;
    }
    Ok(())
}

fn ensure_absolute_path(kind: &str, path: &Path, environment: &str) -> Result<()> {
    ensure(
        ContainerPath::new(path).is_ok(),
        format!("{environment} {kind} must be an absolute normalized path"),
    )
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn valid_environment_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_allowed_host(host: &str) -> bool {
    let host = host.strip_prefix("*.").unwrap_or(host);
    !host.is_empty()
        && !host.contains("://")
        && !host.contains(char::is_whitespace)
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Validation(message.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_start_core::{
        ConfigDocument, EnvironmentManifest as CoreManifest, EnvironmentRegistry, ManifestSource,
    };
    use flate2::read::GzDecoder;

    #[test]
    fn checked_in_assets_validate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is in the repository root");
        validate_repository(root).expect("checked-in assets must validate");
    }

    #[test]
    fn checked_in_manifests_parse_and_resolve_with_core_schema() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is in the repository root");
        let mut registry = EnvironmentRegistry::new();
        for name in REQUIRED_ENVIRONMENTS {
            let path = root.join(format!("assets/environments/{name}.toml"));
            let contents = fs::read_to_string(&path).expect("read built-in manifest");
            let manifest =
                CoreManifest::parse(&contents, path.display().to_string()).expect("parse manifest");
            registry
                .insert(manifest, ManifestSource::built_in(name))
                .expect("register manifest");
        }
        for name in REQUIRED_ENVIRONMENTS {
            let resolved = registry.resolve(name).expect("resolve manifest");
            assert!(resolved.build.is_some());
            assert!(resolved.image.is_none());
        }
    }

    #[test]
    fn documentation_examples_match_core_schemas() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is in the repository root");
        let global_path = root.join("docs/examples/config.toml");
        let global = fs::read_to_string(&global_path).expect("read global example");
        #[cfg(windows)]
        let global = global.replace("/absolute/path/to/", "C:/absolute/path/to/");
        ConfigDocument::parse(&global, global_path.display().to_string())
            .expect("parse global example");

        let project_path = root.join("docs/examples/project.toml");
        let project = fs::read_to_string(&project_path).expect("read project example");
        ConfigDocument::parse(&project, project_path.display().to_string())
            .expect("parse project example")
            .validate_as_project()
            .expect("validate project example");

        let environment_path = root.join("docs/examples/environment.toml");
        let environment = fs::read_to_string(&environment_path).expect("read environment example");
        CoreManifest::parse(&environment, environment_path.display().to_string())
            .expect("parse environment example");
    }

    #[test]
    fn release_checksum_round_trip_and_tamper_detection() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("one.txt"), b"one").expect("write fixture");
        fs::create_dir(temp.path().join("nested")).expect("create fixture directory");
        fs::write(temp.path().join("nested/two.txt"), b"two").expect("write fixture");
        let manifest = temp.path().join("SHA256SUMS");
        write_release_checksums(temp.path(), &manifest).expect("write checksums");
        verify_checksums(&manifest, temp.path()).expect("verify checksums");
        fs::write(temp.path().join("one.txt"), b"changed").expect("tamper fixture");
        assert!(matches!(
            verify_checksums(&manifest, temp.path()),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn release_archives_are_byte_for_byte_deterministic() {
        const EPOCH: u32 = 1_700_000_123;

        let temp = tempfile::tempdir().expect("temporary directory");
        let first_source = temp.path().join("first-source");
        let second_source = temp.path().join("second-source");
        create_release_fixture(&first_source, false);
        create_release_fixture(&second_source, true);
        let first_archive = temp.path().join("first.tar.gz");
        let second_archive = temp.path().join("second.tar.gz");
        let executables = [PathBuf::from("bin/codex-start")];

        write_release_archive(
            &first_source,
            &first_archive,
            Path::new("codex-start-1.0.0"),
            EPOCH,
            &executables,
        )
        .expect("write first archive");
        write_release_archive(
            &second_source,
            &second_archive,
            Path::new("codex-start-1.0.0"),
            EPOCH,
            &executables,
        )
        .expect("write second archive");

        let first = fs::read(&first_archive).expect("read first archive");
        let second = fs::read(&second_archive).expect("read second archive");
        assert_eq!(first, second);
        assert_eq!(&first[4..8], &EPOCH.to_le_bytes());
        assert_eq!(first[3], 0, "gzip header must not contain optional fields");
        assert_eq!(first[9], 255, "gzip operating system must be portable");

        let decoder = GzDecoder::new(first.as_slice());
        let mut archive = tar::Archive::new(decoder);
        let records: Vec<_> = archive
            .entries()
            .expect("read tar entries")
            .map(|entry| {
                let entry = entry.expect("read tar entry");
                (
                    entry.path().expect("read entry path").into_owned(),
                    entry.header().entry_type(),
                    entry.header().mode().expect("read mode"),
                    entry.header().uid().expect("read uid"),
                    entry.header().gid().expect("read gid"),
                    entry.header().mtime().expect("read mtime"),
                )
            })
            .collect();
        let paths: Vec<_> = records
            .iter()
            .map(|(path, _, _, _, _, _)| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "codex-start-1.0.0",
                "codex-start-1.0.0/README.md",
                "codex-start-1.0.0/bin",
                "codex-start-1.0.0/bin/codex-start",
                "codex-start-1.0.0/z.txt",
            ]
        );
        for (_, _, _, uid, gid, mtime) in &records {
            assert_eq!((*uid, *gid, *mtime), (0, 0, u64::from(EPOCH)));
        }
        assert_eq!(records[0].2, 0o755);
        assert_eq!(records[1].2, 0o644);
        assert_eq!(records[2].2, 0o755);
        assert_eq!(records[3].2, 0o755);
        assert_eq!(records[4].2, 0o644);
    }

    #[test]
    fn release_zip_archives_are_byte_for_byte_deterministic() {
        const EPOCH: u32 = 1_700_000_123;

        let temp = tempfile::tempdir().expect("temporary directory");
        let first_source = temp.path().join("first-zip-source");
        let second_source = temp.path().join("second-zip-source");
        create_release_fixture(&first_source, false);
        create_release_fixture(&second_source, true);
        let first_archive = temp.path().join("first.zip");
        let second_archive = temp.path().join("second.zip");
        let executables = [PathBuf::from("bin/codex-start")];

        write_release_archive(
            &first_source,
            &first_archive,
            Path::new("codex-start-1.0.0"),
            EPOCH,
            &executables,
        )
        .expect("write first ZIP archive");
        write_release_archive(
            &second_source,
            &second_archive,
            Path::new("codex-start-1.0.0"),
            EPOCH,
            &executables,
        )
        .expect("write second ZIP archive");

        assert_eq!(
            fs::read(&first_archive).expect("read first ZIP"),
            fs::read(&second_archive).expect("read second ZIP")
        );
        let mut archive = zip::ZipArchive::new(
            fs::File::open(&first_archive).expect("open deterministic ZIP archive"),
        )
        .expect("read deterministic ZIP archive");
        let names: Vec<_> = archive.file_names().map(str::to_owned).collect();
        assert_eq!(
            names,
            [
                "codex-start-1.0.0/",
                "codex-start-1.0.0/README.md",
                "codex-start-1.0.0/bin/",
                "codex-start-1.0.0/bin/codex-start",
                "codex-start-1.0.0/z.txt",
            ]
        );
        let executable = archive
            .by_name("codex-start-1.0.0/bin/codex-start")
            .expect("find executable");
        assert_eq!(executable.unix_mode(), Some(0o100_755));
        assert_eq!(
            executable
                .last_modified()
                .expect("ZIP entry has a timestamp")
                .year(),
            2023
        );
    }

    #[test]
    fn release_manifest_round_trip_and_tamper_detection() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let version = "1.2.3";
        for expected in expected_release_artifacts(version) {
            fs::write(
                temp.path().join(&expected.filename),
                format!("artifact:{}", expected.filename),
            )
            .expect("write release artifact");
            fs::write(
                temp.path().join(format!("{}.bundle", expected.filename)),
                b"signed bundle",
            )
            .expect("write signature bundle");
            fs::write(
                temp.path().join(format!("{}.spdx.json", expected.filename)),
                b"{}",
            )
            .expect("write SBOM");
        }
        let manifest_path = temp.path().join("release-manifest.json");
        write_release_manifest(temp.path(), &manifest_path, version)
            .expect("write release manifest");
        let manifest = load_release_manifest(&manifest_path).expect("load release manifest");
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.version, version);
        assert_eq!(manifest.tag, "v1.2.3");
        assert_eq!(manifest.artifacts.len(), 16);
        validate_release_manifest(&manifest, temp.path()).expect("validate release manifest");

        let json = fs::read_to_string(&manifest_path).expect("read release manifest JSON");
        assert!(json.contains("\"libc\": null"));
        let tampered = temp.path().join(&manifest.artifacts[0].filename);
        fs::write(tampered, b"tampered").expect("tamper release artifact");
        assert!(matches!(
            validate_release_manifest(&manifest, temp.path()),
            Err(Error::Validation(_) | Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn release_manifest_rejects_unknown_fields_and_noncanonical_versions() {
        assert!(validate_semantic_version("1.2.3").is_ok());
        assert!(validate_semantic_version("01.2.3").is_err());
        assert!(validate_semantic_version("v1.2.3").is_err());
        let json = r#"{
          "schema_version": 1,
          "version": "1.2.3",
          "tag": "v1.2.3",
          "artifacts": [],
          "unexpected": true
        }"#;
        assert!(serde_json::from_str::<ReleaseManifest>(json).is_err());
    }

    #[test]
    fn release_archive_paths_and_symlink_targets_cannot_escape() {
        assert_eq!(
            safe_archive_symlink_target(Path::new("bin/tool"), Path::new("../README.md"))
                .expect("target stays inside archive"),
            "../README.md"
        );
        assert!(safe_archive_symlink_target(Path::new("tool"), Path::new("../outside")).is_err());
        assert!(
            safe_archive_symlink_target(Path::new("bin/tool"), Path::new("/etc/passwd")).is_err()
        );
        assert!(
            safe_archive_symlink_target(Path::new("bin/tool"), Path::new("C:/Windows")).is_err()
        );
        assert!(archive_relative_name(Path::new("../release"), "prefix").is_err());
        assert!(archive_relative_name(Path::new("a\nb"), "entry").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn release_archive_preserves_only_safe_relative_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary directory");
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("bin")).expect("create fixture");
        fs::write(source.join("README.md"), b"read me").expect("write fixture");
        symlink("../README.md", source.join("bin/readme")).expect("create safe symlink");
        let output = temp.path().join("safe.tar.gz");
        write_release_archive(&source, &output, Path::new("release"), 42, &[])
            .expect("archive safe symlink");

        let decoder = GzDecoder::new(fs::File::open(&output).expect("open archive"));
        let mut archive = tar::Archive::new(decoder);
        let mut found = false;
        for entry in archive.entries().expect("read entries") {
            let entry = entry.expect("read entry");
            if entry.path().expect("read entry path") == Path::new("release/bin/readme") {
                assert!(entry.header().entry_type().is_symlink());
                assert_eq!(
                    entry.link_name().expect("read link name").as_deref(),
                    Some(Path::new("../README.md"))
                );
                found = true;
            }
        }
        assert!(found);

        symlink("../outside", source.join("escaping")).expect("create escaping symlink");
        assert!(
            write_release_archive(
                &source,
                &temp.path().join("unsafe.tar.gz"),
                Path::new("release"),
                42,
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn digest_validation_rejects_placeholders() {
        assert!(validate_sha256("test", &"0".repeat(64), false).is_ok());
        assert!(validate_sha256("test", "replace-me", false).is_err());
        assert!(validate_sha256("test", "sha256:not-a-digest", true).is_err());
    }

    #[test]
    fn checksum_paths_cannot_escape_the_release_directory() {
        assert!(safe_relative_path(Path::new("nested/archive.tar.gz")));
        assert!(!safe_relative_path(Path::new("../secret")));
        assert!(!safe_relative_path(Path::new("/absolute")));
    }

    fn create_release_fixture(root: &Path, reverse_creation_order: bool) {
        fs::create_dir(root).expect("create fixture root");
        if reverse_creation_order {
            fs::write(root.join("z.txt"), b"last").expect("write fixture");
            fs::create_dir(root.join("bin")).expect("create fixture directory");
            fs::write(root.join("bin/codex-start"), b"binary").expect("write fixture");
            fs::write(root.join("README.md"), b"read me").expect("write fixture");
        } else {
            fs::write(root.join("README.md"), b"read me").expect("write fixture");
            fs::create_dir(root.join("bin")).expect("create fixture directory");
            fs::write(root.join("bin/codex-start"), b"binary").expect("write fixture");
            fs::write(root.join("z.txt"), b"last").expect("write fixture");
        }
        set_fixture_permissions(root, reverse_creation_order);
    }

    #[cfg(unix)]
    fn set_fixture_permissions(root: &Path, reverse: bool) {
        use std::os::unix::fs::PermissionsExt as _;

        let file_mode = if reverse { 0o777 } else { 0o600 };
        let directory_mode = if reverse { 0o700 } else { 0o777 };
        fs::set_permissions(
            root.join("README.md"),
            fs::Permissions::from_mode(file_mode),
        )
        .expect("set fixture permissions");
        fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(directory_mode))
            .expect("set fixture permissions");
    }

    #[cfg(not(unix))]
    fn set_fixture_permissions(_root: &Path, _reverse: bool) {}
}
