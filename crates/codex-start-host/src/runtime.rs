//! Docker and Podman command adapters.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    net::IpAddr,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use clap::ValueEnum;
use codex_start_core::ResourceLimits;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    command::{
        CommandOutput, CommandSpec, IoMode, run_capture, run_checked, run_diagnostic,
        run_interactive,
    },
    error::{HostError, Result},
};

/// Supported OCI command-line runtimes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// Detect a healthy runtime, preferring Docker.
    Auto,
    /// Docker Engine, Docker Desktop, `OrbStack`, or a Docker-compatible CLI.
    Docker,
    /// Podman local or remote client.
    Podman,
}

/// Container mount kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountKind {
    /// Bind a host path.
    Bind,
    /// Attach an engine-managed named volume.
    Volume,
    /// Allocate an in-memory filesystem.
    Tmpfs,
}

/// Portable mount request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountRequest {
    /// Mount kind.
    pub kind: MountKind,
    /// Host path or volume name; absent for tmpfs.
    pub source: Option<OsString>,
    /// Absolute container path.
    pub target: PathBuf,
    /// Prevent writes through this mount.
    pub read_only: bool,
}

/// Portable published-port request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRequest {
    /// Host bind address.
    pub host_ip: IpAddr,
    /// Host port.
    pub host_port: u16,
    /// Container port.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
}

/// Complete workload or sidecar invocation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RunRequest {
    /// Stable container name.
    pub name: String,
    /// OCI image reference.
    pub image: String,
    /// Optional entrypoint override.
    pub entrypoint: Option<String>,
    /// Container command and arguments.
    pub command: Vec<OsString>,
    /// Working directory.
    pub workdir: Option<PathBuf>,
    /// Environment variables. Secret values must instead be mounted as files.
    pub env: BTreeMap<String, OsString>,
    /// Ownership and discovery labels.
    pub labels: BTreeMap<String, String>,
    /// Filesystem mounts.
    pub mounts: Vec<MountRequest>,
    /// Published ports.
    pub publish: Vec<PublishRequest>,
    /// Typed limits for the primary workload. Helper requests leave this empty.
    pub resources: ResourceLimits,
    /// Initial network name or special mode.
    pub network: Option<String>,
    /// Network-scoped alias.
    pub network_alias: Option<String>,
    /// Engine user-namespace mode, when the workload needs an explicit mapping.
    pub user_namespace: Option<String>,
    /// Static container-host mappings.
    pub add_hosts: BTreeMap<String, String>,
    /// Allocate a pseudo-terminal.
    pub tty: bool,
    /// Keep stdin open.
    pub interactive: bool,
    /// Detach immediately.
    pub detach: bool,
    /// Delete container after exit.
    pub remove: bool,
    /// Read-only container root.
    pub read_only: bool,
    /// Run with no Linux capabilities.
    pub drop_all_capabilities: bool,
    /// Re-add narrowly scoped capabilities after the drop set.
    pub add_capabilities: Vec<String>,
    /// Prevent privilege escalation.
    pub no_new_privileges: bool,
    /// Optional numeric or named container user.
    pub user: Option<String>,
    /// Engine-specific expert arguments.
    pub extra_args: Vec<OsString>,
}

/// Image build inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildRequest {
    /// Resulting image reference.
    pub image: String,
    /// Build context.
    pub context: PathBuf,
    /// Dockerfile path.
    pub dockerfile: PathBuf,
    /// Optional multi-stage target.
    pub target: Option<String>,
    /// Build arguments, excluding secrets.
    pub build_args: BTreeMap<String, String>,
    /// Disable layer cache.
    pub no_cache: bool,
}

/// A detected and healthy runtime command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Runtime {
    kind: RuntimeKind,
    program: OsString,
}

/// Non-mutating information reported by the selected engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDetails {
    /// Server version used for the compatibility decision.
    pub server_version: String,
    /// Whether the server runs without root privileges, when reported.
    pub rootless: Option<bool>,
    /// Whether the CLI talks to a remote server, when reported.
    pub remote: Option<bool>,
}

/// Identity mapping selected for the main workload container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadIdentityMode {
    /// Retain the runtime's normal user-namespace and image-user behavior.
    EngineDefault,
    /// Map the rootless Podman service identity to the workload UID/GID.
    RootlessPodmanKeepId,
}

impl RuntimeDetails {
    /// Render a compact diagnostic description.
    #[must_use]
    pub fn summary(&self) -> String {
        let rootless = self.rootless.map_or("rootless mode unknown", |value| {
            if value { "rootless" } else { "rootful" }
        });
        let remote =
            self.remote.map_or(
                "transport unknown",
                |value| {
                    if value { "remote" } else { "local" }
                },
            );
        format!("server {} ({rootless}, {remote})", self.server_version)
    }
}

/// Result of checking the CLI surface without creating engine resources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliCapabilityReport {
    checked_options: usize,
}

impl CliCapabilityReport {
    /// Number of required long options found in command help.
    pub const fn checked_options(self) -> usize {
        self.checked_options
    }
}

/// Result of the disposable runtime capability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeepCapabilityReport {
    /// Whether Docker's special `host-gateway` mapping was exercised.
    pub host_gateway_checked: bool,
    /// Whether rootless Podman's `keep-id` mapping was exercised.
    pub rootless_keep_id_checked: bool,
}

impl DeepCapabilityReport {
    /// Render the features exercised by the probe.
    #[must_use]
    pub fn summary(self) -> &'static str {
        match (self.host_gateway_checked, self.rootless_keep_id_checked) {
            (true, _) => {
                "internal network, labels and filters, named-volume mount, hardened run flags, and Docker host-gateway mapping succeeded"
            }
            (false, true) => {
                "internal network, labels and filters, named-volume mount, hardened run flags, and rootless Podman keep-id mapping succeeded"
            }
            (false, false) => {
                "internal network, labels and filters, named-volume mount, and hardened run flags succeeded (Podman provides its host gateway name)"
            }
        }
    }
}

struct HelpCapability {
    operation: &'static str,
    argv: &'static [&'static str],
    options: &'static [&'static str],
}

const HELP_CAPABILITIES: &[HelpCapability] = &[
    HelpCapability {
        operation: "container run",
        argv: &["run", "--help"],
        options: &[
            "--add-host",
            "--cap-add",
            "--cap-drop",
            "--label",
            "--mount",
            "--network",
            "--network-alias",
            "--read-only",
            "--security-opt",
            "--userns",
        ],
    },
    HelpCapability {
        operation: "network create",
        argv: &["network", "create", "--help"],
        options: &["--internal", "--label"],
    },
    HelpCapability {
        operation: "network connect",
        argv: &["network", "connect", "--help"],
        options: &["--alias"],
    },
    HelpCapability {
        operation: "network list",
        argv: &["network", "ls", "--help"],
        options: &["--filter", "--format"],
    },
    HelpCapability {
        operation: "volume create",
        argv: &["volume", "create", "--help"],
        options: &["--label"],
    },
    HelpCapability {
        operation: "volume list",
        argv: &["volume", "ls", "--help"],
        options: &["--filter", "--format"],
    },
];

impl Runtime {
    /// Detect or validate the requested engine.
    pub fn detect(kind: RuntimeKind, override_program: Option<&OsStr>) -> Result<Self> {
        #[cfg(windows)]
        if kind == RuntimeKind::Podman
            || kind == RuntimeKind::Auto
                && override_program
                    .is_some_and(|program| infer_kind(program) == RuntimeKind::Podman)
        {
            return Err(HostError::Runtime(
                "Podman is not supported by codex-start on Windows; use Docker Desktop with Linux containers"
                    .to_owned(),
            ));
        }
        if let Some(program) = override_program {
            let inferred = match kind {
                RuntimeKind::Auto => infer_kind(program),
                explicit => explicit,
            };
            let runtime = Self {
                kind: inferred,
                program: program.to_owned(),
            };
            runtime.require_healthy()?;
            return Ok(runtime);
        }

        match kind {
            RuntimeKind::Docker => Self::from_program(RuntimeKind::Docker, "docker"),
            RuntimeKind::Podman => Self::from_program(RuntimeKind::Podman, "podman"),
            #[cfg(windows)]
            RuntimeKind::Auto => Self::from_program(RuntimeKind::Docker, "docker").map_err(|error| {
                HostError::Runtime(format!(
                    "no compatible Docker Desktop runtime was found: {error}"
                ))
            }),
            #[cfg(not(windows))]
            RuntimeKind::Auto => match Self::from_program(RuntimeKind::Docker, "docker") {
                Ok(runtime) => Ok(runtime),
                Err(docker_error) => Self::from_program(RuntimeKind::Podman, "podman").map_err(
                    |podman_error| {
                        HostError::Runtime(format!(
                            "no compatible Docker or Podman runtime was found; Docker: {docker_error}; Podman: {podman_error}"
                        ))
                    },
                ),
            },
        }
    }

    fn from_program(kind: RuntimeKind, program: &str) -> Result<Self> {
        let runtime = Self {
            kind,
            program: program.into(),
        };
        runtime.require_healthy()?;
        Ok(runtime)
    }

    /// Engine implementation kind.
    pub const fn kind(&self) -> RuntimeKind {
        self.kind
    }

    /// Exact engine executable selected by detection or an override.
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Engine-provided hostname that resolves to its host/VM gateway.
    pub const fn host_gateway_name(&self) -> &'static str {
        match self.kind {
            RuntimeKind::Podman => "host.containers.internal",
            RuntimeKind::Docker | RuntimeKind::Auto => "host.docker.internal",
        }
    }

    /// Engine-specific host-gateway mapping required on Linux Docker Engine.
    pub fn host_gateway_mapping(&self) -> BTreeMap<String, String> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Auto => {
                BTreeMap::from([("host.docker.internal".to_owned(), "host-gateway".to_owned())])
            }
            RuntimeKind::Podman => BTreeMap::new(),
        }
    }

    /// Query the runtime version and health.
    pub fn version(&self) -> Result<String> {
        let output = run_checked(&CommandSpec::new(&self.program).arg("version"))?;
        Ok(output.stdout_text())
    }

    /// Inspect server mode using structured engine information.
    pub fn details(&self) -> Result<RuntimeDetails> {
        let format = match self.kind {
            RuntimeKind::Podman => "json",
            RuntimeKind::Docker | RuntimeKind::Auto => "{{json .}}",
        };
        let output =
            run_capture(&CommandSpec::new(&self.program).args(["info", "--format", format]))?;
        if !output.status.success() {
            return Err(HostError::Runtime(format!(
                "could not query structured {:?} engine information: {}",
                self.kind,
                diagnostic_text(&output)
            )));
        }
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            HostError::Runtime(format!(
                "{:?} returned invalid JSON from `info --format {format}`: {error}",
                self.kind
            ))
        })?;
        let rootless = find_bool_key(&value, "rootless")
            .or_else(|| contains_string_fragment(&value, "rootless").then_some(true));
        let remote = find_bool_key(&value, "serviceisremote")
            .or_else(|| remote_environment_indicator(self.kind))
            .or_else(|| self.docker_context_remote());
        Ok(RuntimeDetails {
            server_version: self.server_version()?,
            rootless,
            remote,
        })
    }

    /// Apply the identity mapping required by a main workload container.
    ///
    /// A rootless Podman process normally maps the engine service user to
    /// container root. The init helper later drops to the selected workload
    /// UID/GID, which would then be a subordinate identity unable to write a
    /// bind-mounted checkout. `keep-id:uid=...,gid=...` maps the service user
    /// to that exact workload identity. An explicit root user keeps the init
    /// helper privileged inside the namespace long enough to remap the `codex`
    /// account and prepare engine-owned volumes.
    pub fn configure_workload_identity(
        &self,
        request: &mut RunRequest,
        workload_user_id: u32,
        workload_group_id: u32,
    ) -> Result<WorkloadIdentityMode> {
        if self.kind != RuntimeKind::Podman {
            return Ok(WorkloadIdentityMode::EngineDefault);
        }
        let details = self.details()?;
        if details.rootless == Some(false) {
            return Ok(WorkloadIdentityMode::EngineDefault);
        }
        if details.rootless.is_none() {
            return Err(HostError::Runtime(
                "Podman did not report whether its server is rootless; refusing to guess a workspace bind-mount identity mapping"
                    .to_owned(),
            ));
        }
        if request.user.is_some()
            || request.user_namespace.is_some()
            || request
                .extra_args
                .iter()
                .any(|argument| identity_option(argument))
        {
            return Err(HostError::Config(
                "rootless Podman reserves --user, --userns, UID/GID-map, and subordinate-ID runtime arguments so codex-start can preserve writable workspace mounts"
                    .to_owned(),
            ));
        }
        request.user_namespace = Some(format!(
            "keep-id:uid={workload_user_id},gid={workload_group_id}"
        ));
        request.user = Some("0:0".to_owned());
        Ok(WorkloadIdentityMode::RootlessPodmanKeepId)
    }

    /// Verify required long options without creating containers or resources.
    pub fn capability_report(&self) -> Result<CliCapabilityReport> {
        let mut checked_options = 0;
        for capability in HELP_CAPABILITIES {
            let output = run_capture(
                &CommandSpec::new(&self.program).args(capability.argv.iter().copied()),
            )?;
            if !output.status.success() {
                return Err(HostError::Runtime(format!(
                    "could not inspect {:?} {} capabilities with `{}`: {}",
                    self.kind,
                    capability.operation,
                    rendered_invocation(&self.program, capability.argv),
                    diagnostic_text(&output)
                )));
            }
            let help = String::from_utf8_lossy(&output.stdout);
            let missing = capability
                .options
                .iter()
                .copied()
                .filter(|option| !help_has_option(&help, option))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(HostError::Runtime(format!(
                    "{:?} {} lacks required option(s) {}; install a compatible Docker 27+ or Podman 5.4+ runtime",
                    self.kind,
                    capability.operation,
                    missing.join(", ")
                )));
            }
            checked_options += capability.options.len();
        }
        Ok(CliCapabilityReport { checked_options })
    }

    /// Exercise engine-backed features with uniquely labelled disposable resources.
    pub fn deep_capability_probe(&self, image: &str) -> Result<DeepCapabilityReport> {
        let id = Uuid::new_v4().simple().to_string();
        let suffix = &id[..12];
        let network = format!("codex-start-probe-net-{suffix}");
        let volume = format!("codex-start-probe-vol-{suffix}");
        let container = format!("codex-start-probe-run-{suffix}");
        let labels = BTreeMap::from([
            ("io.codex-start.managed".to_owned(), "true".to_owned()),
            ("io.codex-start.probe".to_owned(), id.clone()),
        ]);

        let result =
            self.run_deep_capability_probe(image, &id, &network, &volume, &container, &labels);
        let cleanup = self.cleanup_capability_probe(&id, &network, &volume, &container);
        match (result, cleanup) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(HostError::Runtime(format!(
                "{error}; capability-probe cleanup also failed: {cleanup_error}"
            ))),
        }
    }

    fn require_healthy(&self) -> Result<()> {
        let version = self.server_version()?;
        let minimum = match self.kind {
            RuntimeKind::Docker | RuntimeKind::Auto => (27, 0),
            RuntimeKind::Podman => (5, 4),
        };
        let found = parse_runtime_version(&version).ok_or_else(|| {
            HostError::Runtime(format!(
                "could not parse {:?} server version {version:?}",
                self.kind
            ))
        })?;
        if found < minimum {
            return Err(HostError::Runtime(format!(
                "{:?} server {version} is unsupported; version {}.{} or newer is required",
                self.kind, minimum.0, minimum.1
            )));
        }
        self.capability_report()?;
        Ok(())
    }

    fn server_version(&self) -> Result<String> {
        let templates: &[&str] = match self.kind {
            RuntimeKind::Docker | RuntimeKind::Auto => &["{{.Server.Version}}"],
            RuntimeKind::Podman => &["{{.Server.Version}}", "{{.Version}}"],
        };
        for template in templates {
            let output = run_capture(
                &CommandSpec::new(&self.program).args(["version", "--format", *template]),
            )?;
            let value = output.stdout_text();
            if output.status.success() && parse_runtime_version(&value).is_some() {
                return Ok(value);
            }
        }
        Err(HostError::Runtime(format!(
            "could not query a healthy {:?} server through {}",
            self.kind,
            self.program.to_string_lossy()
        )))
    }

    fn docker_context_remote(&self) -> Option<bool> {
        if !matches!(self.kind, RuntimeKind::Docker | RuntimeKind::Auto) {
            return None;
        }
        let output =
            run_capture(&CommandSpec::new(&self.program).args(["context", "show"])).ok()?;
        if !output.status.success() {
            return None;
        }
        let context = output.stdout_text();
        (!context.is_empty()).then_some(context != "default")
    }

    fn run_deep_capability_probe(
        &self,
        image: &str,
        id: &str,
        network: &str,
        volume: &str,
        container: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<DeepCapabilityReport> {
        self.create_network(network, true, labels)
            .map_err(|error| capability_error("create a labelled internal network", &error))?;
        if !self.network_is_internal(network)? {
            return Err(HostError::Runtime(format!(
                "runtime capability probe network {network:?} was not marked internal"
            )));
        }
        self.require_probe_label("network", network, id)?;
        self.ensure_volume(volume, labels)
            .map_err(|error| capability_error("create a labelled named volume", &error))?;
        self.require_probe_label("volume", volume, id)?;
        let filtered_networks = self.list_network_names(&format!("io.codex-start.probe={id}"))?;
        require_filtered_resource("network", network, &filtered_networks)?;
        let filtered_volumes = self.list_volume_names(&format!("io.codex-start.probe={id}"))?;
        require_filtered_resource("volume", volume, &filtered_volumes)?;

        let host_gateway_checked = matches!(self.kind, RuntimeKind::Docker | RuntimeKind::Auto);
        let mut request = RunRequest {
            name: container.to_owned(),
            image: image.to_owned(),
            entrypoint: Some("/bin/true".to_owned()),
            labels: labels.clone(),
            mounts: vec![MountRequest {
                kind: MountKind::Volume,
                source: Some(OsString::from(volume)),
                target: PathBuf::from("/tmp/codex-start-runtime-probe"),
                read_only: true,
            }],
            network: Some(network.to_owned()),
            network_alias: Some("capability-probe".to_owned()),
            add_hosts: self.host_gateway_mapping(),
            remove: true,
            read_only: true,
            drop_all_capabilities: true,
            no_new_privileges: true,
            ..RunRequest::default()
        };
        let identity_mode = self.configure_workload_identity(&mut request, 1_000, 1_000)?;
        let status = self
            .run(&request)
            .map_err(|error| capability_error("run a hardened disposable container", &error))?;
        if status != 0 {
            return Err(HostError::Runtime(format!(
                "runtime capability probe container exited {status}; verify read-only roots, capability dropping, no-new-privileges, mounts, networks, and host-gateway support"
            )));
        }
        Ok(DeepCapabilityReport {
            host_gateway_checked,
            rootless_keep_id_checked: identity_mode == WorkloadIdentityMode::RootlessPodmanKeepId,
        })
    }

    fn network_is_internal(&self, network: &str) -> Result<bool> {
        let output =
            run_capture(&CommandSpec::new(&self.program).args(["network", "inspect", network]))?;
        if !output.status.success() {
            return Err(HostError::Runtime(format!(
                "could not inspect capability-probe network {network:?}: {}",
                diagnostic_text(&output)
            )));
        }
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            HostError::Runtime(format!(
                "runtime returned invalid JSON while inspecting network {network:?}: {error}"
            ))
        })?;
        find_bool_key(&value, "internal").ok_or_else(|| {
            HostError::Runtime(format!(
                "runtime did not report whether capability-probe network {network:?} is internal"
            ))
        })
    }

    fn require_probe_label(&self, resource: &str, name: &str, id: &str) -> Result<()> {
        let actual = self.inspect_label(
            resource,
            name,
            "{{ index .Labels \"io.codex-start.probe\" }}",
        )?;
        if actual.as_deref() == Some(id) {
            Ok(())
        } else {
            Err(HostError::Runtime(format!(
                "runtime did not preserve the capability-probe label on {resource} {name:?}; found {actual:?}"
            )))
        }
    }

    fn cleanup_capability_probe(
        &self,
        id: &str,
        network: &str,
        volume: &str,
        container: &str,
    ) -> Result<()> {
        let mut failures = Vec::new();
        match self.container_label(container, "io.codex-start.probe") {
            Ok(Some(owner)) if owner == id => {
                if let Err(error) = self.remove_container(container, true) {
                    failures.push(format!("container {container:?}: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!(
                "could not verify container {container:?} ownership: {error}"
            )),
        }
        match self.volume_label(volume, "io.codex-start.probe") {
            Ok(Some(owner)) if owner == id => {
                if let Err(error) = self.remove_volume(volume, true) {
                    failures.push(format!("volume {volume:?}: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!(
                "could not verify volume {volume:?} ownership: {error}"
            )),
        }
        match self.network_label(network, "io.codex-start.probe") {
            Ok(Some(owner)) if owner == id => {
                if let Err(error) = self.remove_network(network) {
                    failures.push(format!("network {network:?}: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!(
                "could not verify network {network:?} ownership: {error}"
            )),
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Runtime(format!(
                "could not clean disposable runtime probe resources: {}",
                failures.join("; ")
            )))
        }
    }

    /// Test whether an image is already present.
    pub fn image_exists(&self, image: &str) -> Result<bool> {
        let output = run_capture(
            &CommandSpec::new(&self.program)
                .args(["image", "inspect", image])
                .io(IoMode::Null),
        )?;
        Ok(output.status.success())
    }

    /// Build an image while reserving stdout for the launched workload.
    pub fn build(&self, request: &BuildRequest) -> Result<u8> {
        let command = self.build_command(request);
        run_diagnostic(&command)
    }

    /// Render the exact build command.
    pub fn build_command(&self, request: &BuildRequest) -> CommandSpec {
        let mut command = CommandSpec::new(&self.program).arg("build");
        command = command
            .args([OsString::from("--tag"), OsString::from(&request.image)])
            .args([
                OsString::from("--file"),
                request.dockerfile.as_os_str().to_owned(),
            ]);
        if let Some(target) = &request.target {
            command = command.args([OsString::from("--target"), OsString::from(target)]);
        }
        if request.no_cache {
            command = command.arg("--no-cache");
        }
        for (key, value) in &request.build_args {
            command = command.args([
                OsString::from("--build-arg"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        command
            .arg(request.context.as_os_str())
            .io(IoMode::Diagnostic)
    }

    /// Pull an OCI image while reserving stdout for the launched workload.
    pub fn pull(&self, image: &str) -> Result<u8> {
        run_diagnostic(&self.pull_command(image))
    }

    /// Render the exact pull command.
    pub fn pull_command(&self, image: &str) -> CommandSpec {
        CommandSpec::new(&self.program)
            .args(["pull", image])
            .io(IoMode::Diagnostic)
    }

    /// Create an owned network.
    pub fn create_network(
        &self,
        name: &str,
        internal: bool,
        labels: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut command = CommandSpec::new(&self.program).args(["network", "create"]);
        if internal {
            command = command.arg("--internal");
        }
        for (key, value) in labels {
            command = command.args(["--label", &format!("{key}={value}")]);
        }
        run_checked(&command.arg(name)).map(|_| ())
    }

    /// Attach a running container to an additional network.
    pub fn connect_network(&self, network: &str, container: &str, alias: &str) -> Result<()> {
        run_checked(
            &CommandSpec::new(&self.program)
                .args(["network", "connect", "--alias", alias, network, container]),
        )
        .map(|_| ())
    }

    /// Remove a network when present.
    pub fn remove_network(&self, name: &str) -> Result<()> {
        let output = run_capture(&CommandSpec::new(&self.program).args(["network", "rm", name]))?;
        if output.status.success() || String::from_utf8_lossy(&output.stderr).contains("not found")
        {
            Ok(())
        } else {
            output.require_success(&self.program).map(|_| ())
        }
    }

    /// Execute a complete run request and return the child exit code.
    pub fn run(&self, request: &RunRequest) -> Result<u8> {
        let command = self.run_command(request);
        if request.detach {
            Ok(crate::command::exit_code(run_capture(&command)?.status))
        } else {
            run_interactive(&command)
        }
    }

    /// Render a portable request as exact engine arguments.
    pub fn run_command(&self, request: &RunRequest) -> CommandSpec {
        let mut command = CommandSpec::new(&self.program).arg("run");
        if request.remove {
            command = command.arg("--rm");
        }
        if request.detach {
            command = command.arg("--detach");
        }
        if request.tty {
            command = command.arg("--tty");
        }
        if request.interactive {
            command = command.arg("--interactive");
        }
        command = command.args(["--name", &request.name]);
        if let Some(network) = &request.network {
            command = command.args(["--network", network]);
        }
        if let Some(alias) = &request.network_alias {
            command = command.args(["--network-alias", alias]);
        }
        if let Some(user_namespace) = &request.user_namespace {
            command = command.args(["--userns", user_namespace]);
        }
        for (host, address) in &request.add_hosts {
            command = command.args([
                OsString::from("--add-host"),
                OsString::from(format!("{host}:{address}")),
            ]);
        }
        if let Some(workdir) = &request.workdir {
            command = command.arg("--workdir").arg(workdir.as_os_str());
        }
        if let Some(entrypoint) = &request.entrypoint {
            command = command.args(["--entrypoint", entrypoint]);
        }
        if let Some(user) = &request.user {
            command = command.args(["--user", user]);
        }
        if request.read_only {
            command = command.arg("--read-only");
        }
        if request.drop_all_capabilities {
            command = command.args(["--cap-drop", "ALL"]);
        }
        for capability in &request.add_capabilities {
            command = command.args(["--cap-add", capability]);
        }
        if request.no_new_privileges {
            command = command.args(["--security-opt", "no-new-privileges"]);
        }
        for (key, value) in &request.labels {
            command = command.args([
                OsString::from("--label"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        for (key, value) in &request.env {
            command = command.args([
                OsString::from("--env"),
                prefixed_os_value(key.as_bytes(), b'=', value),
            ]);
        }
        for mount in &request.mounts {
            command = command.args([OsString::from("--mount"), render_mount(mount)]);
        }
        for publish in &request.publish {
            command = command.args([
                OsString::from("--publish"),
                OsString::from(format_publish_address(
                    publish.host_ip,
                    publish.host_port,
                    publish.container_port,
                    &publish.protocol,
                )),
            ]);
        }
        command = append_resource_args(command, &request.resources);
        command = command.args(request.extra_args.clone());
        command = command.arg(&request.image).args(request.command.clone());
        if request.detach {
            command.io(IoMode::Capture)
        } else {
            command.io(IoMode::Inherit)
        }
    }

    /// Inspect whether a container exists and is running.
    pub fn container_state(&self, name: &str) -> Result<Option<bool>> {
        let output = run_capture(&CommandSpec::new(&self.program).args([
            "container",
            "inspect",
            "--format",
            "{{.State.Running}}",
            name,
        ]))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(output.stdout_text() == "true"))
    }

    /// Remove a container when it exists.
    pub fn remove_container(&self, name: &str, force: bool) -> Result<()> {
        if self.container_state(name)?.is_none() {
            return Ok(());
        }
        let mut command = CommandSpec::new(&self.program).args(["container", "rm"]);
        if force {
            command = command.arg("--force");
        }
        run_checked(&command.arg(name)).map(|_| ())
    }

    /// Stop a running container.
    pub fn stop_container(&self, name: &str) -> Result<()> {
        run_checked(&CommandSpec::new(&self.program).args(["container", "stop", name])).map(|_| ())
    }

    /// Start an existing stopped container without attaching to its streams.
    pub fn start_container(&self, name: &str) -> Result<()> {
        run_checked(&CommandSpec::new(&self.program).args(["container", "start", name])).map(|_| ())
    }

    /// Attach the current terminal to a container without proxying host signals.
    ///
    /// Disabling signal proxying ensures a terminal hangup disconnects the
    /// client rather than terminating the session's primary process.
    pub fn attach(&self, name: &str) -> Result<u8> {
        run_interactive(&CommandSpec::new(&self.program).args([
            "attach",
            "--sig-proxy=false",
            name,
        ]))
    }

    /// Read the exit code of a stopped container when the engine reports one.
    pub fn container_exit_code(&self, name: &str) -> Result<Option<u8>> {
        let output = run_capture(&CommandSpec::new(&self.program).args([
            "container",
            "inspect",
            "--format",
            "{{.State.ExitCode}}",
            name,
        ]))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(output
            .stdout_text()
            .parse::<u16>()
            .ok()
            .and_then(|value| u8::try_from(value).ok()))
    }

    /// Run an interactive command in an existing container.
    pub fn exec(
        &self,
        name: &str,
        workdir: Option<&Path>,
        argv: &[OsString],
        tty: bool,
    ) -> Result<u8> {
        let mut command = CommandSpec::new(&self.program).args(["exec", "--interactive"]);
        if tty {
            command = command.arg("--tty");
        }
        if let Some(workdir) = workdir {
            command = command.arg("--workdir").arg(workdir.as_os_str());
        }
        command = command.arg(name).args(argv.iter().cloned());
        run_interactive(&command)
    }

    /// Run a non-interactive probe in an existing container without emitting output.
    pub fn exec_probe(&self, name: &str, argv: &[OsString]) -> Result<bool> {
        let command = CommandSpec::new(&self.program)
            .args(["exec", name])
            .args(argv.iter().cloned())
            .io(IoMode::Null);
        Ok(run_capture(&command)?.status.success())
    }

    /// Fetch logs for a container.
    pub fn logs(&self, name: &str, follow: bool) -> Result<u8> {
        let mut command = CommandSpec::new(&self.program).arg("logs");
        if follow {
            command = command.arg("--follow");
        }
        run_interactive(&command.arg(name))
    }

    /// List containers matching an ownership label as JSON-ish rows.
    pub fn list_containers(&self, label: &str, all: bool) -> Result<CommandOutput> {
        let mut command = CommandSpec::new(&self.program).arg("ps");
        if all {
            command = command.arg("--all");
        }
        run_checked(&command.args([
            "--filter",
            &format!("label={label}"),
            "--format",
            "{{json .}}",
        ]))
    }

    /// Read one label from a container, returning `None` when absent or unknown.
    pub fn container_label(&self, name: &str, label: &str) -> Result<Option<String>> {
        self.inspect_label(
            "container",
            name,
            &format!("{{{{ index .Config.Labels {label:?} }}}}"),
        )
    }

    /// Read one label from a network, returning `None` when absent or unknown.
    pub fn network_label(&self, name: &str, label: &str) -> Result<Option<String>> {
        self.inspect_label(
            "network",
            name,
            &format!("{{{{ index .Labels {label:?} }}}}"),
        )
    }

    /// Read one label from a volume, returning `None` when absent or unknown.
    pub fn volume_label(&self, name: &str, label: &str) -> Result<Option<String>> {
        self.inspect_label(
            "volume",
            name,
            &format!("{{{{ index .Labels {label:?} }}}}"),
        )
    }

    fn inspect_label(&self, resource: &str, name: &str, template: &str) -> Result<Option<String>> {
        let output = run_capture(
            &CommandSpec::new(&self.program)
                .args([resource, "inspect", "--format", template, name]),
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = output.stdout_text();
        Ok((!value.is_empty() && value != "<no value>").then_some(value))
    }

    /// List owned network names by label.
    pub fn list_network_names(&self, label: &str) -> Result<Vec<String>> {
        let output = run_checked(&CommandSpec::new(&self.program).args([
            "network",
            "ls",
            "--filter",
            &format!("label={label}"),
            "--format",
            "{{.Name}}",
        ]))?;
        Ok(output
            .stdout_text()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Ensure an engine-managed volume exists and carries ownership labels.
    pub fn ensure_volume(&self, name: &str, labels: &BTreeMap<String, String>) -> Result<()> {
        let inspect =
            run_capture(&CommandSpec::new(&self.program).args(["volume", "inspect", name]))?;
        if inspect.status.success() {
            for (key, expected) in labels {
                let actual = self.volume_label(name, key)?;
                if actual.as_deref() != Some(expected) {
                    return Err(HostError::Runtime(format!(
                        "refusing to reuse volume {name:?}: ownership label {key:?} is {actual:?}, expected {expected:?}"
                    )));
                }
            }
            return Ok(());
        }
        let mut command = CommandSpec::new(&self.program).args(["volume", "create"]);
        for (key, value) in labels {
            command = command.args(["--label", &format!("{key}={value}")]);
        }
        run_checked(&command.arg(name)).map(|_| ())
    }

    /// List volume names by ownership label.
    pub fn list_volume_names(&self, label: &str) -> Result<Vec<String>> {
        let output = run_checked(&CommandSpec::new(&self.program).args([
            "volume",
            "ls",
            "--filter",
            &format!("label={label}"),
            "--format",
            "{{.Name}}",
        ]))?;
        Ok(output
            .stdout_text()
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Remove a named volume when present.
    pub fn remove_volume(&self, name: &str, force: bool) -> Result<()> {
        let mut command = CommandSpec::new(&self.program).args(["volume", "rm"]);
        if force {
            command = command.arg("--force");
        }
        let output = run_capture(&command.arg(name))?;
        if output.status.success()
            || String::from_utf8_lossy(&output.stderr).contains("not found")
            || String::from_utf8_lossy(&output.stderr).contains("no such volume")
        {
            Ok(())
        } else {
            output.require_success(&self.program).map(|_| ())
        }
    }
}

fn append_resource_args(mut command: CommandSpec, resources: &ResourceLimits) -> CommandSpec {
    if let Some(cpus) = &resources.cpus {
        command = command.args(["--cpus", cpus.as_engine_value()]);
    }
    if let Some(cpu_shares) = resources.cpu_shares {
        command = command.args([
            OsString::from("--cpu-shares"),
            OsString::from(cpu_shares.to_string()),
        ]);
    }
    if let Some(cpuset_cpus) = &resources.cpuset_cpus {
        command = command.args(["--cpuset-cpus", cpuset_cpus]);
    }
    if let Some(memory) = &resources.memory {
        command = command.args(["--memory", memory.as_engine_value()]);
    }
    if let Some(memory_reservation) = &resources.memory_reservation {
        command = command.args(["--memory-reservation", memory_reservation.as_engine_value()]);
    }
    if let Some(memory_swap) = &resources.memory_swap {
        command = command.args(["--memory-swap", memory_swap.as_engine_value()]);
    }
    if let Some(pids_limit) = resources.pids_limit {
        command = command.args([
            OsString::from("--pids-limit"),
            OsString::from(pids_limit.to_string()),
        ]);
    }
    if let Some(shm_size) = &resources.shm_size {
        command = command.args(["--shm-size", shm_size.as_engine_value()]);
    }
    for (name, limit) in &resources.ulimits {
        command = command.args([
            OsString::from("--ulimit"),
            OsString::from(format!("{name}={}", limit.as_engine_value())),
        ]);
    }
    command
}

/// Render an engine port specification, bracketing IPv6 bind addresses.
#[must_use]
pub fn format_publish_address(
    host_ip: IpAddr,
    host_port: u16,
    container_port: u16,
    protocol: &str,
) -> String {
    match host_ip {
        IpAddr::V4(address) => {
            format!("{address}:{host_port}:{container_port}/{protocol}")
        }
        IpAddr::V6(address) => {
            format!("[{address}]:{host_port}:{container_port}/{protocol}")
        }
    }
}

fn parse_runtime_version(value: &str) -> Option<(u64, u64)> {
    let value = value.trim().trim_start_matches('v');
    let mut components = value.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components
        .next()?
        .bytes()
        .take_while(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    let minor = std::str::from_utf8(&minor).ok()?.parse().ok()?;
    Some((major, minor))
}

fn help_has_option(help: &str, expected: &str) -> bool {
    help.split_ascii_whitespace().any(|token| {
        let token =
            token.trim_matches(|character: char| matches!(character, ',' | '[' | ']' | '(' | ')'));
        token.split_once('=').map_or(token, |(option, _)| option) == expected
    })
}

fn find_bool_key(value: &Value, expected: &str) -> Option<bool> {
    match value {
        Value::Object(entries) => entries.iter().find_map(|(key, value)| {
            if key.eq_ignore_ascii_case(expected) {
                value.as_bool()
            } else {
                find_bool_key(value, expected)
            }
        }),
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_bool_key(value, expected)),
        _ => None,
    }
}

fn contains_string_fragment(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
        Value::Object(entries) => entries
            .values()
            .any(|value| contains_string_fragment(value, expected)),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_string_fragment(value, expected)),
        _ => false,
    }
}

fn remote_environment_indicator(kind: RuntimeKind) -> Option<bool> {
    if std::env::var_os("CONTAINER_CONNECTION").is_some_and(|value| !value.is_empty()) {
        return Some(true);
    }
    let variable = match kind {
        RuntimeKind::Podman => "CONTAINER_HOST",
        RuntimeKind::Docker | RuntimeKind::Auto => "DOCKER_HOST",
    };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(|value| kind == RuntimeKind::Podman || !value.to_string_lossy().starts_with("unix://"))
}

fn identity_option(argument: &OsStr) -> bool {
    let bytes = argument.as_encoded_bytes();
    [
        b"--user".as_slice(),
        b"--userns".as_slice(),
        b"--uidmap".as_slice(),
        b"--gidmap".as_slice(),
        b"--subuidname".as_slice(),
        b"--subgidname".as_slice(),
    ]
    .iter()
    .any(|option| {
        bytes == *option
            || bytes
                .strip_prefix(*option)
                .is_some_and(|suffix| suffix.starts_with(b"="))
    }) || bytes == b"-u"
        || (bytes.starts_with(b"-u") && bytes.len() > 2)
}

fn rendered_invocation(program: &OsStr, argv: &[&str]) -> String {
    std::iter::once(program.to_string_lossy().into_owned())
        .chain(argv.iter().map(|argument| (*argument).to_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn diagnostic_text(output: &CommandOutput) -> String {
    const LIMIT: usize = 2_048;
    let bytes = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let mut value = String::from_utf8_lossy(bytes).trim().to_owned();
    if value.len() > LIMIT {
        let mut boundary = LIMIT;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
        value.push('…');
    }
    if value.is_empty() {
        format!("command exited {} without diagnostics", output.status)
    } else {
        value
    }
}

fn capability_error(operation: &str, error: &HostError) -> HostError {
    HostError::Runtime(format!(
        "could not {operation} during runtime capability probe: {error}"
    ))
}

fn require_filtered_resource(resource: &str, expected: &str, values: &[String]) -> Result<()> {
    if values.iter().any(|value| value == expected) {
        Ok(())
    } else {
        Err(HostError::Runtime(format!(
            "runtime {resource} label filter did not return disposable {resource} {expected:?}"
        )))
    }
}

fn infer_kind(program: &OsStr) -> RuntimeKind {
    let name = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .to_ascii_lowercase();
    if name.contains("podman") {
        RuntimeKind::Podman
    } else {
        RuntimeKind::Docker
    }
}

#[cfg(unix)]
fn render_mount(mount: &MountRequest) -> OsString {
    let kind = match mount.kind {
        MountKind::Bind => "bind",
        MountKind::Volume => "volume",
        MountKind::Tmpfs => "tmpfs",
    };
    let mut rendered = format!("type={kind}").into_bytes();
    if let Some(source) = &mount.source {
        rendered.extend_from_slice(b",src=");
        append_mount_value(&mut rendered, source.as_bytes());
    }
    rendered.extend_from_slice(b",dst=");
    append_mount_value(&mut rendered, mount.target.as_os_str().as_bytes());
    if mount.read_only {
        rendered.extend_from_slice(b",readonly");
    }
    OsString::from_vec(rendered)
}

#[cfg(windows)]
fn render_mount(mount: &MountRequest) -> OsString {
    let kind = match mount.kind {
        MountKind::Bind => "bind",
        MountKind::Volume => "volume",
        MountKind::Tmpfs => "tmpfs",
    };
    let mut rendered = format!("type={kind}");
    if let Some(source) = &mount.source {
        rendered.push_str(",src=");
        append_mount_text(&mut rendered, &source.to_string_lossy());
    }
    rendered.push_str(",dst=");
    let target = mount.target.to_string_lossy().replace('\\', "/");
    append_mount_text(&mut rendered, &target);
    if mount.read_only {
        rendered.push_str(",readonly");
    }
    rendered.into()
}

#[cfg(unix)]
fn prefixed_os_value(prefix: &[u8], separator: u8, value: &OsStr) -> OsString {
    let mut rendered = Vec::with_capacity(prefix.len() + value.as_bytes().len() + 1);
    rendered.extend_from_slice(prefix);
    rendered.push(separator);
    rendered.extend_from_slice(value.as_bytes());
    OsString::from_vec(rendered)
}

#[cfg(windows)]
fn prefixed_os_value(prefix: &[u8], separator: u8, value: &OsStr) -> OsString {
    let mut rendered = OsString::from(String::from_utf8_lossy(prefix).as_ref());
    rendered.push(char::from(separator).to_string());
    rendered.push(value);
    rendered
}

#[cfg(unix)]
fn append_mount_value(rendered: &mut Vec<u8>, value: &[u8]) {
    if value.iter().any(|byte| matches!(byte, b',' | b'"')) {
        rendered.push(b'"');
        for byte in value {
            rendered.push(*byte);
            if *byte == b'"' {
                rendered.push(b'"');
            }
        }
        rendered.push(b'"');
    } else {
        rendered.extend_from_slice(value);
    }
}

#[cfg(windows)]
fn append_mount_text(rendered: &mut String, value: &str) {
    if value.contains([',', '"']) {
        rendered.push('"');
        rendered.push_str(&value.replace('"', "\"\""));
        rendered.push('"');
    } else {
        rendered.push_str(value);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        fs,
        net::Ipv4Addr,
        os::unix::ffi::{OsStrExt, OsStringExt},
        os::unix::fs::PermissionsExt,
        path::PathBuf,
    };

    use super::{
        BuildRequest, MountKind, MountRequest, PublishRequest, RunRequest, Runtime, RuntimeKind,
        WorkloadIdentityMode, help_has_option, identity_option, parse_runtime_version,
    };
    use crate::command::IoMode;

    const ALL_REQUIRED_OPTIONS: &str = "--add-host --cap-add --cap-drop --label --mount --network \
        --network-alias --read-only --security-opt --userns --internal --alias --filter --format";

    fn runtime(kind: RuntimeKind) -> Runtime {
        Runtime {
            kind,
            program: match kind {
                RuntimeKind::Podman => "podman".into(),
                RuntimeKind::Docker | RuntimeKind::Auto => "docker".into(),
            },
        }
    }

    #[test]
    fn renders_portable_run_command_without_a_shell() {
        let request = RunRequest {
            name: "codex-test".to_owned(),
            image: "example/image:locked".to_owned(),
            command: vec!["codex".into(), "hello world".into()],
            workdir: Some(PathBuf::from("/workspaces/test")),
            mounts: vec![MountRequest {
                kind: MountKind::Bind,
                source: Some("/tmp/project".into()),
                target: PathBuf::from("/workspaces/test"),
                read_only: false,
            }],
            publish: vec![PublishRequest {
                host_ip: Ipv4Addr::LOCALHOST.into(),
                host_port: 5173,
                container_port: 5173,
                protocol: "tcp".to_owned(),
            }],
            remove: true,
            interactive: true,
            tty: true,
            labels: BTreeMap::from([("io.codex-start.managed".to_owned(), "true".to_owned())]),
            drop_all_capabilities: true,
            add_capabilities: vec!["SETUID".to_owned(), "SETGID".to_owned()],
            ..RunRequest::default()
        };
        let command = runtime(RuntimeKind::Docker).run_command(&request);
        assert!(command.args.windows(2).any(|pair| {
            pair == [
                std::ffi::OsString::from("--mount"),
                std::ffi::OsString::from("type=bind,src=/tmp/project,dst=/workspaces/test"),
            ]
        }));
        assert!(
            command
                .args
                .windows(4)
                .any(|arguments| { arguments == ["--cap-add", "SETUID", "--cap-add", "SETGID",] })
        );
        assert!(command.args.windows(2).any(|pair| {
            pair == [
                std::ffi::OsString::from("--publish"),
                std::ffi::OsString::from("127.0.0.1:5173:5173/tcp"),
            ]
        }));
        assert_eq!(
            &command.args[command.args.len() - 3..],
            ["example/image:locked", "codex", "hello world"]
        );
    }

    #[test]
    fn renders_typed_resources_identically_for_docker_and_podman() {
        let request = RunRequest {
            name: "resource-test".to_owned(),
            image: "example/image:locked".to_owned(),
            resources: toml::from_str(
                r#"
                cpus = 2.5
                cpu_shares = 1024
                cpuset_cpus = "0-3"
                memory = "8g"
                memory_reservation = "4g"
                memory_swap = "10g"
                pids_limit = 512
                shm_size = "1g"
                [ulimits]
                memlock = "-1:-1"
                nofile = "65536:65536"
                "#,
            )
            .expect("resource limits"),
            ..RunRequest::default()
        };
        let expected = [
            "run",
            "--name",
            "resource-test",
            "--cpus",
            "2.5",
            "--cpu-shares",
            "1024",
            "--cpuset-cpus",
            "0-3",
            "--memory",
            "8g",
            "--memory-reservation",
            "4g",
            "--memory-swap",
            "10g",
            "--pids-limit",
            "512",
            "--shm-size",
            "1g",
            "--ulimit",
            "memlock=-1:-1",
            "--ulimit",
            "nofile=65536:65536",
            "example/image:locked",
        ];
        for kind in [RuntimeKind::Docker, RuntimeKind::Podman] {
            assert_eq!(runtime(kind).run_command(&request).args, expected);
        }
    }

    #[test]
    fn parses_runtime_versions_with_prefixes_and_suffixes() {
        assert_eq!(parse_runtime_version("v27.5.1"), Some((27, 5)));
        assert_eq!(parse_runtime_version("5.4.2-dev"), Some((5, 4)));
        assert_eq!(parse_runtime_version("unknown"), None);
    }

    #[test]
    fn recognizes_exact_long_help_options() {
        assert!(help_has_option("  -f, --filter filter", "--filter"));
        assert!(help_has_option("[--mount=mount]", "--mount"));
        assert!(!help_has_option("--mount-label string", "--mount"));
        assert!(!help_has_option(
            "documentation mentions --filtering",
            "--filter"
        ));
    }

    #[test]
    fn mocked_adapter_accepts_required_cli_surface_and_structured_info() {
        let (root, executable) = fake_runtime(ALL_REQUIRED_OPTIONS);
        let runtime = Runtime::detect(RuntimeKind::Docker, Some(executable.as_os_str())).unwrap();
        assert_eq!(runtime.capability_report().unwrap().checked_options(), 18);
        let details = runtime.details().unwrap();
        assert_eq!(details.server_version, "29.0.0");
        assert_eq!(details.rootless, Some(true));
        drop(root);
    }

    #[test]
    fn mocked_adapter_reports_the_operation_and_missing_option() {
        let options = ALL_REQUIRED_OPTIONS.replace("--mount", "");
        let (_root, executable) = fake_runtime(&options);
        let error = Runtime::detect(RuntimeKind::Docker, Some(executable.as_os_str()))
            .expect_err("missing mount support must fail");
        let message = error.to_string();
        assert!(message.contains("container run"));
        assert!(message.contains("--mount"));
        assert!(message.contains("Docker 27+ or Podman 5.4+"));
    }

    #[test]
    fn local_rootless_podman_keeps_host_id_while_starting_init_as_root() {
        let (root, executable) = fake_podman(true, false);
        let runtime = Runtime::detect(RuntimeKind::Podman, Some(executable.as_os_str())).unwrap();
        let mut request = RunRequest {
            name: "codex-rootless".to_owned(),
            image: "example/image:locked".to_owned(),
            entrypoint: Some("/usr/local/bin/codex-start-init".to_owned()),
            ..RunRequest::default()
        };

        assert_eq!(
            runtime
                .configure_workload_identity(&mut request, 4_242, 4_343)
                .unwrap(),
            WorkloadIdentityMode::RootlessPodmanKeepId
        );
        assert_eq!(
            request.user_namespace.as_deref(),
            Some("keep-id:uid=4242,gid=4343")
        );
        assert_eq!(request.user.as_deref(), Some("0:0"));
        assert_eq!(
            runtime.run_command(&request).args,
            [
                "run",
                "--name",
                "codex-rootless",
                "--userns",
                "keep-id:uid=4242,gid=4343",
                "--entrypoint",
                "/usr/local/bin/codex-start-init",
                "--user",
                "0:0",
                "example/image:locked",
            ]
        );
        drop(root);
    }

    #[test]
    fn rootful_podman_retains_engine_identity_defaults() {
        let (root, executable) = fake_podman(false, false);
        let runtime = Runtime::detect(RuntimeKind::Podman, Some(executable.as_os_str())).unwrap();
        let mut request = RunRequest {
            name: "codex-default".to_owned(),
            image: "example/image:locked".to_owned(),
            ..RunRequest::default()
        };
        assert_eq!(
            runtime
                .configure_workload_identity(&mut request, 1_001, 121)
                .unwrap(),
            WorkloadIdentityMode::EngineDefault
        );
        assert!(request.user_namespace.is_none());
        assert!(request.user.is_none());
        drop(root);
    }

    #[test]
    fn remote_rootless_podman_maps_service_user_to_client_workload_id() {
        let (root, executable) = fake_podman(true, true);
        let runtime = Runtime::detect(RuntimeKind::Podman, Some(executable.as_os_str())).unwrap();
        let mut request = RunRequest {
            name: "codex-remote".to_owned(),
            image: "example/image:locked".to_owned(),
            ..RunRequest::default()
        };
        assert_eq!(
            runtime
                .configure_workload_identity(&mut request, 1_000, 1_000)
                .unwrap(),
            WorkloadIdentityMode::RootlessPodmanKeepId
        );
        assert_eq!(
            request.user_namespace.as_deref(),
            Some("keep-id:uid=1000,gid=1000")
        );
        assert_eq!(request.user.as_deref(), Some("0:0"));
        drop(root);
    }

    #[test]
    fn rootless_podman_rejects_identity_overrides_that_break_init() {
        let (root, executable) = fake_podman(true, false);
        let runtime = Runtime::detect(RuntimeKind::Podman, Some(executable.as_os_str())).unwrap();
        for arguments in [
            vec![OsString::from("--userns=host")],
            vec![OsString::from("--user"), OsString::from("1000")],
            vec![OsString::from("--uidmap=0:1:1")],
            vec![OsString::from("-u1000")],
        ] {
            let mut request = RunRequest {
                name: "codex-conflict".to_owned(),
                image: "example/image:locked".to_owned(),
                extra_args: arguments,
                ..RunRequest::default()
            };
            let error = runtime
                .configure_workload_identity(&mut request, 1_000, 1_000)
                .expect_err("identity override must be rejected");
            assert!(error.to_string().contains("reserves --user"));
        }
        drop(root);
    }

    #[test]
    fn recognizes_only_identity_altering_runtime_options() {
        for argument in [
            "--user",
            "--user=0",
            "--userns=keep-id",
            "--uidmap",
            "--gidmap=0:0:1",
            "--subuidname=codex",
            "--subgidname",
            "-u",
            "-u1000",
        ] {
            assert!(
                identity_option(std::ffi::OsStr::new(argument)),
                "{argument}"
            );
        }
        for argument in ["--userland-proxy", "--unsetenv", "--annotation"] {
            assert!(
                !identity_option(std::ffi::OsStr::new(argument)),
                "{argument}"
            );
        }
    }

    #[test]
    fn brackets_ipv6_publish_addresses() {
        assert_eq!(
            super::format_publish_address("::1".parse().unwrap(), 8080, 80, "tcp"),
            "[::1]:8080:80/tcp"
        );
        assert_eq!(
            super::format_publish_address("127.0.0.1".parse().unwrap(), 8080, 80, "udp"),
            "127.0.0.1:8080:80/udp"
        );
    }

    #[test]
    fn renders_build_target_and_args() {
        let request = BuildRequest {
            image: "codex-start:hash".to_owned(),
            context: PathBuf::from("images"),
            dockerfile: PathBuf::from("images/Dockerfile"),
            target: Some("rust".to_owned()),
            build_args: BTreeMap::from([("CODEX_VERSION".to_owned(), "0.1".to_owned())]),
            no_cache: false,
        };
        let command = runtime(RuntimeKind::Podman).build_command(&request);
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--target", "rust"])
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--build-arg", "CODEX_VERSION=0.1"])
        );
        assert!(!command.args.iter().any(|argument| argument == "--load"));
        assert_eq!(command.io, IoMode::Diagnostic);
    }

    #[test]
    fn pull_diagnostics_do_not_share_workload_stdout() {
        let command = runtime(RuntimeKind::Docker).pull_command("example/image:locked");
        assert_eq!(command.args, ["pull", "example/image:locked"]);
        assert_eq!(command.io, IoMode::Diagnostic);
    }

    #[test]
    fn preserves_non_utf8_environment_and_mount_paths() {
        let env_value = OsString::from_vec(vec![b'a', 0xFF, b'b']);
        let mount_source = OsString::from_vec(vec![b'/', b't', 0xFE, b',', b'x']);
        let request = RunRequest {
            name: "codex-exact".to_owned(),
            image: "example/image:locked".to_owned(),
            env: BTreeMap::from([("EXACT".to_owned(), env_value)]),
            mounts: vec![MountRequest {
                kind: MountKind::Bind,
                source: Some(mount_source),
                target: PathBuf::from("/workspace"),
                read_only: false,
            }],
            ..RunRequest::default()
        };
        let command = runtime(RuntimeKind::Docker).run_command(&request);
        let env = command
            .args
            .windows(2)
            .find(|pair| pair[0] == "--env")
            .expect("environment argument");
        assert_eq!(
            env[1].as_bytes(),
            &[b'E', b'X', b'A', b'C', b'T', b'=', b'a', 0xFF, b'b']
        );
        let mount = command
            .args
            .windows(2)
            .find(|pair| pair[0] == "--mount")
            .expect("mount argument");
        assert_eq!(
            mount[1].as_bytes(),
            b"type=bind,src=\"/t\xFE,x\",dst=/workspace"
        );
    }

    fn fake_runtime(help: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mock-docker");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = version ]; then echo 29.0.0; exit 0; fi\n\
             if [ \"$1\" = info ]; then echo '{{\"SecurityOptions\":[\"name=rootless\"]}}'; exit 0; fi\n\
             case \"$*\" in *--help*) echo '{help}'; exit 0;; esac\n\
             exit 1\n"
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (root, executable)
    }

    fn fake_podman(rootless: bool, remote: bool) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mock-podman");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = version ]; then echo 5.4.0; exit 0; fi\n\
             if [ \"$1\" = info ]; then echo '{{\"host\":{{\"security\":{{\"rootless\":{rootless}}},\"serviceIsRemote\":{remote}}}}}'; exit 0; fi\n\
             case \"$*\" in *--help*) echo '{ALL_REQUIRED_OPTIONS}'; exit 0;; esac\n\
             exit 1\n"
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (root, executable)
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::{MountKind, MountRequest, Runtime, RuntimeKind, render_mount};

    fn rendered(source: &str) -> OsString {
        render_mount(&MountRequest {
            kind: MountKind::Bind,
            source: Some(source.into()),
            target: PathBuf::from("/workspaces/project"),
            read_only: true,
        })
    }

    #[test]
    fn renders_windows_bind_sources_without_changing_container_paths() {
        assert_eq!(
            rendered(r"C:\Users\Codex Project"),
            r"type=bind,src=C:\Users\Codex Project,dst=/workspaces/project,readonly"
        );
        assert_eq!(
            rendered(r"C:\Users\Codex,Project"),
            r#"type=bind,src="C:\Users\Codex,Project",dst=/workspaces/project,readonly"#
        );
        assert_eq!(
            rendered(r"\\server\share\project"),
            r"type=bind,src=\\server\share\project,dst=/workspaces/project,readonly"
        );
        assert_eq!(
            rendered(r"\\?\C:\very long\project"),
            r"type=bind,src=\\?\C:\very long\project,dst=/workspaces/project,readonly"
        );
    }

    #[test]
    fn rejects_podman_before_launching_a_process() {
        let explicit = Runtime::detect(RuntimeKind::Podman, None).unwrap_err();
        assert!(explicit.to_string().contains("Podman is not supported"));
        let inferred = Runtime::detect(RuntimeKind::Auto, Some("podman.exe".as_ref())).unwrap_err();
        assert!(inferred.to_string().contains("Podman is not supported"));
    }
}
