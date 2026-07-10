//! Per-run isolated networks and Rust egress-sidecar lifecycle.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use codex_start_core::{NetworkMode, NetworkPlan, ProxyConfig, ProxyPlan};
use codex_start_proxy::container_init::{ExecSpec, InitSpec};
use codex_start_proxy::container_init::{HttpProxyServiceSpec, InitServiceSpec};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    content_hash,
    error::{HostError, Result},
    paths::{create_private_dir, set_private_file},
    runtime::{BuildRequest, MountKind, MountRequest, RunRequest, Runtime},
};

const PROXY_ALIAS: &str = "codex-start-proxy";
const AUTH_CONTAINER_DIRECTORY: &str = "/run/codex-start/secrets/egress";
const AUTH_CONTAINER_FILE: &str = "/run/codex-start/secrets/egress/token";
const AUTH_CONTAINER_MAP: &str = "/run/codex-start/secrets/egress/map.json";
const SIDECAR_SPEC_FILE: &str = "/run/codex-start/secrets/egress/sidecar-spec.json";
const SIDECAR_TOKEN_ENV: &str = "CODEX_START_EGRESS_TOKEN";
const MANAGED_LABEL: &str = "io.codex-start.managed";
const PROJECT_LABEL: &str = "io.codex-start.project";

/// Runtime resources constraining workload egress.
#[derive(Debug)]
pub struct NetworkSession<'a> {
    runtime: &'a Runtime,
    /// Workload `--network` value, if any.
    pub workload_network: Option<String>,
    /// HTTP proxy URL injected into allowlisted workloads.
    pub proxy_url: Option<String>,
    network_name: Option<String>,
    outer_network_name: Option<String>,
    sidecar_name: Option<String>,
    sidecar_image: Option<String>,
    proxy_port: u16,
    labels: BTreeMap<String, String>,
}

/// Per-run authentication material shared by the egress sidecar and the
/// workload's loopback proxy bridge.
#[derive(Debug)]
pub struct EgressAuthentication {
    directory: TempDir,
}

impl EgressAuthentication {
    /// Generate a private printable bearer token without retaining its value in
    /// launcher state.
    pub fn create(runtime_parent: &Path) -> Result<Self> {
        create_private_dir(runtime_parent)?;
        let directory = tempfile::Builder::new()
            .prefix("egress-auth-")
            .tempdir_in(runtime_parent)
            .map_err(|source| HostError::io(runtime_parent, source))?;
        create_private_dir(directory.path())?;
        let token_path = directory.path().join("token");
        let token = Zeroizing::new(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_path)
            .map_err(|source| HostError::io(&token_path, source))?;
        file.write_all(token.as_bytes())
            .map_err(|source| HostError::io(&token_path, source))?;
        file.sync_all()
            .map_err(|source| HostError::io(&token_path, source))?;
        set_private_file(&token_path)?;
        let map_path = directory.path().join("map.json");
        let map = serde_json::to_vec(&json!({ (SIDECAR_TOKEN_ENV): AUTH_CONTAINER_FILE }))
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        write_private_file(&map_path, &map)?;
        Ok(Self { directory })
    }

    /// Read-only bind mount whose source path contains no token value.
    #[must_use]
    pub fn mount(&self) -> MountRequest {
        MountRequest {
            kind: MountKind::Bind,
            source: Some(self.directory.path().as_os_str().to_owned()),
            target: PathBuf::from(AUTH_CONTAINER_DIRECTORY),
            read_only: true,
        }
    }

    /// Stable container token path used in init and sidecar argv.
    #[must_use]
    pub fn container_token_file() -> PathBuf {
        PathBuf::from(AUTH_CONTAINER_FILE)
    }

    fn write_sidecar_spec(&self, command: Vec<OsString>) -> Result<()> {
        let command = ExecSpec::from_argv(command)
            .map_err(|error| HostError::Config(format!("invalid sidecar command: {error}")))?;
        let spec = InitSpec {
            version: 1,
            uid: Some(65_532),
            gid: Some(65_532),
            account: None,
            cwd: None,
            clear_environment: false,
            env: BTreeMap::new(),
            secret_map: Some(PathBuf::from(AUTH_CONTAINER_MAP)),
            secret_root: PathBuf::from(AUTH_CONTAINER_DIRECTORY),
            allow_insecure_secret_permissions: false,
            ownership_paths: Vec::new(),
            ssh: None,
            prepare: Vec::new(),
            services: Vec::new(),
            command,
        };
        codex_start_proxy::container_init::validate_spec(&spec).map_err(|error| {
            HostError::Config(format!("invalid generated sidecar init spec: {error}"))
        })?;
        let bytes = serde_json::to_vec_pretty(&spec)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        write_private_file(&self.directory.path().join("sidecar-spec.json"), &bytes)
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| HostError::io(path, source))?;
    file.write_all(contents)
        .map_err(|source| HostError::io(path, source))?;
    file.sync_all()
        .map_err(|source| HostError::io(path, source))?;
    set_private_file(path)
}

/// Complete inputs for one managed network boundary.
pub struct NetworkOptions<'a> {
    pub assets_root: &'a Path,
    pub sidecar_build_args: &'a BTreeMap<String, String>,
    pub mode: NetworkMode,
    pub run_name: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    pub allow_hosts: &'a [String],
    pub allow_private: &'a [String],
    pub proxy: &'a ProxyConfig,
    pub authentication: Option<&'a EgressAuthentication>,
    pub rebuild_sidecar: bool,
}

impl<'a> NetworkSession<'a> {
    /// Create network resources and, for allowlist mode, start the Rust proxy.
    pub fn start(runtime: &'a Runtime, options: &NetworkOptions<'_>) -> Result<Self> {
        match options.mode {
            NetworkMode::Bridge => Ok(Self {
                runtime,
                workload_network: None,
                proxy_url: None,
                network_name: None,
                outer_network_name: None,
                sidecar_name: None,
                sidecar_image: None,
                proxy_port: options.proxy.listen_port,
                labels: options.labels.clone(),
            }),
            NetworkMode::Host => Ok(Self {
                runtime,
                workload_network: Some("host".to_owned()),
                proxy_url: None,
                network_name: None,
                outer_network_name: None,
                sidecar_name: None,
                sidecar_image: None,
                proxy_port: options.proxy.listen_port,
                labels: options.labels.clone(),
            }),
            NetworkMode::Offline | NetworkMode::Allowlist => Self::start_isolated(runtime, options),
        }
    }

    fn start_isolated(runtime: &'a Runtime, options: &NetworkOptions<'_>) -> Result<Self> {
        let network_name = limited_name(&format!("{}-net", options.run_name));
        remove_owned_network(runtime, &network_name, options.labels)?;
        runtime.create_network(&network_name, true, options.labels)?;
        let mut session = Self {
            runtime,
            workload_network: Some(network_name.clone()),
            proxy_url: None,
            network_name: Some(network_name),
            outer_network_name: None,
            sidecar_name: None,
            sidecar_image: None,
            proxy_port: options.proxy.listen_port,
            labels: options.labels.clone(),
        };
        if options.mode == NetworkMode::Offline {
            return Ok(session);
        }
        if options.allow_hosts.is_empty() {
            return Err(HostError::Config(
                "allowlist mode requires at least one allowed host".to_owned(),
            ));
        }
        if options.authentication.is_none() {
            return Err(HostError::Runtime(
                "allowlist mode requires per-run proxy authentication".to_owned(),
            ));
        }
        let outer_network = limited_name(&format!("{}-egress-net", options.run_name));
        remove_owned_network(runtime, &outer_network, options.labels)?;
        runtime.create_network(&outer_network, false, options.labels)?;
        session.outer_network_name = Some(outer_network);
        let image = ensure_sidecar_image(
            runtime,
            options.assets_root,
            options.sidecar_build_args,
            options.rebuild_sidecar,
        )?;
        let sidecar_name = limited_name(&format!("{}-proxy", options.run_name));
        remove_owned_container(runtime, &sidecar_name, options.labels)?;
        session.sidecar_name = Some(sidecar_name.clone());
        session.sidecar_image = Some(image.clone());
        let request = sidecar_request(runtime, options, sidecar_name.clone(), image)?;
        if runtime.run(&request)? != 0 {
            return Err(HostError::Runtime(
                "egress sidecar failed to start".to_owned(),
            ));
        }
        runtime.connect_network(
            session
                .network_name
                .as_deref()
                .ok_or_else(|| HostError::Runtime("managed network disappeared".to_owned()))?,
            &sidecar_name,
            PROXY_ALIAS,
        )?;
        wait_for_health(runtime, &sidecar_name, options.proxy.listen_port)?;
        session.proxy_url = Some(format!("http://127.0.0.1:{}", options.proxy.listen_port));
        Ok(session)
    }

    /// Runtime-neutral description of the resources owned by this session.
    pub fn logical_plan(
        &self,
        mode: NetworkMode,
        allow_hosts: &[String],
        allow_private: &[String],
    ) -> Result<NetworkPlan> {
        match mode {
            NetworkMode::Bridge => Ok(NetworkPlan::bridge()),
            NetworkMode::Host => Ok(NetworkPlan::host()),
            NetworkMode::Offline => self
                .network_name
                .clone()
                .map(NetworkPlan::offline)
                .ok_or_else(|| HostError::Runtime("offline network was not created".to_owned())),
            NetworkMode::Allowlist => {
                let network_name = self.network_name.clone().ok_or_else(|| {
                    HostError::Runtime("allowlist network was not created".to_owned())
                })?;
                let proxy = ProxyPlan {
                    name: self.sidecar_name.clone().ok_or_else(|| {
                        HostError::Runtime("egress sidecar was not created".to_owned())
                    })?,
                    image: self.sidecar_image.clone().ok_or_else(|| {
                        HostError::Runtime("egress sidecar image is unavailable".to_owned())
                    })?,
                    network_name: network_name.clone(),
                    egress_network_name: self.outer_network_name.clone().ok_or_else(|| {
                        HostError::Runtime("egress network was not created".to_owned())
                    })?,
                    listen_port: self.proxy_port,
                    allow_hosts: allow_hosts.to_vec(),
                    private_service_hosts: allow_private.to_vec(),
                    authentication_required: true,
                    read_only: true,
                    cap_drop: vec!["ALL".to_owned()],
                    cap_add: vec!["SETUID".to_owned(), "SETGID".to_owned()],
                };
                Ok(NetworkPlan::allowlist(network_name, proxy))
            }
        }
    }

    /// Explicitly remove owned resources and surface cleanup failures.
    pub fn cleanup(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        if let Some(sidecar) = self.sidecar_name.take()
            && let Err(error) = remove_owned_container(self.runtime, &sidecar, &self.labels)
        {
            failures.push(error.to_string());
        }
        if let Some(network) = self.network_name.take()
            && let Err(error) = remove_owned_network(self.runtime, &network, &self.labels)
        {
            failures.push(error.to_string());
        }
        if let Some(network) = self.outer_network_name.take()
            && let Err(error) = remove_owned_network(self.runtime, &network, &self.labels)
        {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Runtime(format!(
                "network cleanup failed: {}",
                failures.join("; ")
            )))
        }
    }

    /// Loopback bridge inserted into the workload init service list.
    #[must_use]
    pub fn workload_proxy_service(&self, proxy: &ProxyConfig) -> Option<InitServiceSpec> {
        self.proxy_url.as_ref().map(|_| {
            InitServiceSpec::HttpProxy(HttpProxyServiceSpec {
                listen: ([127, 0, 0, 1], proxy.listen_port).into(),
                proxy: format!("{PROXY_ALIAS}:{}", proxy.listen_port),
                auth_token_file: EgressAuthentication::container_token_file(),
                max_connections: proxy.max_connections,
                connect_timeout_seconds: proxy.connect_timeout_seconds,
                handshake_timeout_seconds: proxy.header_timeout_seconds,
                idle_timeout_seconds: proxy.idle_timeout_seconds,
                max_header_bytes: proxy.max_header_bytes,
            })
        })
    }
}

fn remove_owned_network(
    runtime: &Runtime,
    name: &str,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let managed = runtime.network_label(name, MANAGED_LABEL)?;
    if managed.is_none() {
        return Ok(());
    }
    require_ownership(
        name,
        managed.as_deref(),
        runtime.network_label(name, PROJECT_LABEL)?.as_deref(),
        expected,
    )?;
    runtime.remove_network(name)
}

fn remove_owned_container(
    runtime: &Runtime,
    name: &str,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    if runtime.container_state(name)?.is_none() {
        return Ok(());
    }
    require_ownership(
        name,
        runtime.container_label(name, MANAGED_LABEL)?.as_deref(),
        runtime.container_label(name, PROJECT_LABEL)?.as_deref(),
        expected,
    )?;
    runtime.remove_container(name, true)
}

fn require_ownership(
    name: &str,
    managed: Option<&str>,
    project: Option<&str>,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let expected_project = expected.get(PROJECT_LABEL).map(String::as_str);
    if managed == Some("true") && project == expected_project {
        Ok(())
    } else {
        Err(HostError::Runtime(format!(
            "refusing to replace resource {name:?}: codex-start ownership could not be proven"
        )))
    }
}

impl Drop for NetworkSession<'_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn sidecar_request(
    runtime: &Runtime,
    options: &NetworkOptions<'_>,
    name: String,
    image: String,
) -> Result<RunRequest> {
    let mut labels = options.labels.clone();
    labels.insert("io.codex-start.role".to_owned(), "egress".to_owned());
    let authentication = options
        .authentication
        .expect("allowlist authentication was validated before sidecar planning");
    let mut sidecar_argv = vec![OsString::from("/usr/local/bin/codex-start-sidecar")];
    sidecar_argv.extend(sidecar_command(options));
    authentication.write_sidecar_spec(sidecar_argv)?;
    Ok(RunRequest {
        name,
        image,
        entrypoint: Some("/usr/local/bin/codex-start-init".to_owned()),
        command: vec![
            OsString::from("run"),
            OsString::from("--spec"),
            OsString::from(SIDECAR_SPEC_FILE),
        ],
        labels,
        mounts: vec![authentication.mount()],
        network: Some(limited_name(&format!("{}-egress-net", options.run_name))),
        add_hosts: runtime.host_gateway_mapping(),
        detach: true,
        read_only: true,
        drop_all_capabilities: true,
        // Docker preserves host ownership on bind mounts.  The init process
        // starts as container root but has no ownership of the private host
        // authentication directory, so it needs this read-only bypass long
        // enough to load its spec and token.  It then execs the sidecar as
        // the unprivileged identity specified in the init spec.
        add_capabilities: vec![
            "DAC_READ_SEARCH".to_owned(),
            "SETUID".to_owned(),
            "SETGID".to_owned(),
        ],
        no_new_privileges: true,
        user: Some("0:0".to_owned()),
        ..RunRequest::default()
    })
}

fn sidecar_command(options: &NetworkOptions<'_>) -> Vec<OsString> {
    let mut command = vec![
        OsString::from("egress"),
        OsString::from("--listen"),
        OsString::from(format!("0.0.0.0:{}", options.proxy.listen_port)),
        OsString::from("--max-connections"),
        OsString::from(options.proxy.max_connections.to_string()),
        OsString::from("--connect-timeout-seconds"),
        OsString::from(options.proxy.connect_timeout_seconds.to_string()),
        OsString::from("--header-timeout-seconds"),
        OsString::from(options.proxy.header_timeout_seconds.to_string()),
        OsString::from("--max-header-bytes"),
        OsString::from(options.proxy.max_header_bytes.to_string()),
        OsString::from("--idle-timeout-seconds"),
        OsString::from(options.proxy.idle_timeout_seconds.to_string()),
        OsString::from("--auth-token-env"),
        OsString::from(SIDECAR_TOKEN_ENV),
    ];
    for host in options.allow_hosts {
        command.extend([OsString::from("--allow"), OsString::from(host)]);
    }
    for host in options.allow_private {
        command.extend([OsString::from("--allow-private"), OsString::from(host)]);
    }
    command
}

/// Build or reuse the content-addressed Rust sidecar image.
pub fn ensure_sidecar_image(
    runtime: &Runtime,
    root: &Path,
    build_args: &BTreeMap<String, String>,
    rebuild: bool,
) -> Result<String> {
    let image = sidecar_image_tag(root, build_args)?;
    let root = root.to_path_buf();
    let dockerfile = root.join("images/sidecar/Dockerfile");
    if rebuild || !runtime.image_exists(&image)? {
        let request = BuildRequest {
            image: image.clone(),
            context: root,
            dockerfile,
            target: None,
            build_args: build_args.clone(),
            no_cache: rebuild,
        };
        if runtime.build(&request)? != 0 {
            return Err(HostError::Runtime("sidecar image build failed".to_owned()));
        }
    }
    Ok(image)
}

/// Compute the sidecar image tag without contacting a container engine.
pub fn sidecar_image_tag(root: &Path, build_args: &BTreeMap<String, String>) -> Result<String> {
    let dockerfile = root.join("images/sidecar/Dockerfile");
    let mut hasher = blake3::Hasher::new();
    for path in [
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
        dockerfile.clone(),
    ] {
        content_hash::hash_entry(root, &path, &mut hasher)?;
    }
    content_hash::hash_tree(
        root,
        &root.join("crates/codex-start-proxy"),
        &mut hasher,
        |_| false,
    )?;
    content_hash::hash_tree(
        root,
        &root.join("crates/codex-start-core"),
        &mut hasher,
        |_| false,
    )?;
    for (key, value) in build_args {
        hasher.update(&(key.len() as u64).to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize().to_hex();
    Ok(format!("codex-start-sidecar:{}", &digest[..16]))
}

fn wait_for_health(runtime: &Runtime, container: &str, proxy_port: u16) -> Result<()> {
    let argv = [
        OsString::from("codex-start-sidecar"),
        OsString::from("healthcheck"),
        OsString::from("--proxy"),
        OsString::from(format!("127.0.0.1:{proxy_port}")),
        OsString::from("--timeout-seconds"),
        OsString::from("1"),
    ];
    for _ in 0..50 {
        if runtime.exec_probe(container, &argv).unwrap_or(false) {
            let identity = [
                OsString::from("codex-start-sidecar"),
                OsString::from("identity-check"),
                OsString::from("--uid"),
                OsString::from("65532"),
                OsString::from("--gid"),
                OsString::from("65532"),
            ];
            if runtime.exec_probe(container, &identity).unwrap_or(false) {
                return Ok(());
            }
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = runtime.logs(container, false);
    Err(HostError::Runtime(format!(
        "egress sidecar {container} did not become healthy"
    )))
}

pub(crate) fn limited_name(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.len() <= 63 {
        trimmed.to_owned()
    } else {
        let digest = blake3::hash(value.as_bytes()).to_hex();
        format!("{}-{}", &trimmed[..46], &digest[..16])
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, fs, os::unix::fs::PermissionsExt};

    use codex_start_proxy::container_init::InitSpec;

    use super::{EgressAuthentication, limited_name, sidecar_image_tag};

    #[test]
    fn egress_token_stays_out_of_spec_and_final_sidecar_identity_is_non_root() {
        let runtime = tempfile::tempdir().expect("runtime");
        let authentication = EgressAuthentication::create(runtime.path()).expect("auth");
        authentication
            .write_sidecar_spec(vec![
                OsString::from("codex-start-sidecar"),
                OsString::from("egress"),
                OsString::from("--auth-token-env"),
                OsString::from("CODEX_START_EGRESS_TOKEN"),
            ])
            .expect("spec");
        let token = fs::read(authentication.directory.path().join("token")).expect("token");
        let spec_bytes =
            fs::read(authentication.directory.path().join("sidecar-spec.json")).expect("spec");
        assert!(
            !spec_bytes
                .windows(token.len())
                .any(|window| window == token)
        );
        let spec: InitSpec = serde_json::from_slice(&spec_bytes).expect("valid spec");
        assert_eq!(spec.uid.zip(spec.gid), Some((65_532, 65_532)));
        assert_eq!(
            spec.command.argv(),
            [
                "codex-start-sidecar",
                "egress",
                "--auth-token-env",
                "CODEX_START_EGRESS_TOKEN",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            fs::metadata(authentication.directory.path().join("token"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn resource_names_are_bounded_and_collision_resistant() {
        let long = "Project With A Very Long Name That Exceeds Every Container Runtime Name Recommendation By Far";
        let name = limited_name(long);
        assert!(name.len() <= 63);
        assert!(name.chars().all(|character| character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '-'));
        assert_ne!(name, limited_name(&format!("{long}!")));
    }

    #[test]
    fn sidecar_tag_tracks_entry_type_target_and_mode() {
        let root = tempfile::tempdir().expect("root");
        let root = root.path();
        for directory in [
            "images/sidecar",
            "crates/codex-start-proxy/src",
            "crates/codex-start-core/src",
        ] {
            fs::create_dir_all(root.join(directory)).expect("directory");
        }
        for (path, contents) in [
            ("Cargo.toml", "[workspace]\n"),
            ("Cargo.lock", "version = 4\n"),
            ("images/sidecar/Dockerfile", "FROM scratch\n"),
            ("crates/codex-start-core/src/lib.rs", "pub fn core() {}\n"),
            ("crates/codex-start-proxy/src/lib.rs", "pub fn proxy() {}\n"),
            (
                "crates/codex-start-proxy/src/same.rs",
                "pub fn proxy() {}\n",
            ),
            (
                "crates/codex-start-proxy/src/other.rs",
                "pub fn proxy() {}\n",
            ),
        ] {
            fs::write(root.join(path), contents).expect("file");
        }

        let arguments = BTreeMap::new();
        let source = root.join("crates/codex-start-proxy/src/lib.rs");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).expect("mode");
        let regular = sidecar_image_tag(root, &arguments).expect("regular tag");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("mode");
        let executable = sidecar_image_tag(root, &arguments).expect("executable tag");
        assert_ne!(regular, executable);

        fs::remove_file(&source).expect("remove regular");
        std::os::unix::fs::symlink("same.rs", &source).expect("symlink");
        let first_target = sidecar_image_tag(root, &arguments).expect("first symlink tag");
        assert_ne!(regular, first_target);
        fs::remove_file(&source).expect("remove symlink");
        std::os::unix::fs::symlink("other.rs", &source).expect("symlink");
        let second_target = sidecar_image_tag(root, &arguments).expect("second symlink tag");
        assert_ne!(first_target, second_target);
    }

    #[test]
    fn sidecar_tag_rejects_symlinks_outside_the_context() {
        let root = tempfile::tempdir().expect("root");
        let root = root.path();
        for directory in [
            "images/sidecar",
            "crates/codex-start-proxy/src",
            "crates/codex-start-core/src",
        ] {
            fs::create_dir_all(root.join(directory)).expect("directory");
        }
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "images/sidecar/Dockerfile",
            "crates/codex-start-core/src/lib.rs",
        ] {
            fs::write(root.join(path), "content\n").expect("file");
        }
        fs::write(root.join("outside.rs"), "content\n").expect("outside");
        std::os::unix::fs::symlink(
            "../../../../outside.rs",
            root.join("crates/codex-start-proxy/src/lib.rs"),
        )
        .expect("symlink");

        let error = sidecar_image_tag(root, &BTreeMap::new()).expect_err("unsafe symlink");
        assert!(matches!(error, crate::error::HostError::UnsafePath { .. }));
    }
}
