//! Typed bridge between the runtime-neutral domain plan and an engine request.
//!
//! The types in this module are also the canonical dry-run representation. They
//! deliberately contain secret references and mounted paths, but never resolved
//! secret contents.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    net::IpAddr,
    path::PathBuf,
};

use codex_start_core::{
    ContainerPath, ContainerPlan, ContainerPlanError, HostServiceSpec, MountPlan, MountSource,
    NetworkMode, NetworkPlan, PortProtocol, PublishedPort, ResourceLimits, RuntimeKind,
    UnixArgument,
};
use codex_start_proxy::container_init::{
    CommandSpec as PrepareCommand, ExecSpec, InitServiceSpec, InitSpec,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    forwarding::ForwardingPlan,
    host_services::HostServicePlan,
    runtime::{MountKind as RuntimeMountKind, MountRequest, PublishRequest, RunRequest},
};

/// Current serialized host launch-plan schema.
pub const HOST_LAUNCH_PLAN_SCHEMA_VERSION: u32 = 1;

/// Complete validated launch description, including host-only engine details.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLaunchPlan {
    pub schema_version: u32,
    /// Runtime-neutral ownership, security, and workload description.
    pub container: ContainerPlan,
    /// Exact engine invocation. This may wrap the workload with the init binary.
    pub runtime: RuntimePlan,
    /// Initialization work performed before executing the workload.
    #[serde(default)]
    pub init: InitPlan,
    /// Redacted summary of forwarded host facilities.
    #[serde(default)]
    pub forwarding: ForwardingMetadata,
    /// Host endpoints intentionally exposed to the workload.
    #[serde(default)]
    pub host_services: HostServiceMetadata,
    /// Persistent lifecycle selected for this invocation.
    #[serde(default)]
    pub session: SessionMetadata,
}

/// Redacted persistent-session behavior included in dry-run plans.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub kind: Option<PlannedSessionKind>,
    #[serde(default)]
    pub reboot_replay: bool,
    #[serde(default)]
    pub refresh_ssh_on_attach: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Persistent execution shape chosen from the raw Codex invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedSessionKind {
    Interactive,
    Job,
}

/// Host-engine fields that are absent from, or intentionally more exact than,
/// [`ContainerPlan`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlan {
    /// Exact `--entrypoint` value used by the engine.
    #[serde(default)]
    pub entrypoint: Option<UnixArgument>,
    /// Exact command passed to the selected image.
    #[serde(default)]
    pub command: Vec<UnixArgument>,
    /// Engine working-directory override. `None` retains the image default.
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    /// Complete non-secret environment, preserving non-UTF-8 Unix values.
    #[serde(default)]
    pub env: BTreeMap<String, UnixArgument>,
    /// Exact engine network selector.
    #[serde(default)]
    pub network: Option<String>,
    /// Optional alias within the selected network.
    #[serde(default)]
    pub network_alias: Option<String>,
    /// Exact engine user-namespace selector.
    #[serde(default)]
    pub user_namespace: Option<String>,
    /// Static name-to-address mappings.
    #[serde(default)]
    pub add_hosts: BTreeMap<String, String>,
    /// Run in the background.
    #[serde(default)]
    pub detach: bool,
    /// Expert engine arguments, represented without shell parsing or UTF-8 loss.
    #[serde(default)]
    pub extra_args: Vec<UnixArgument>,
}

/// Init-helper configuration safe to expose in a dry-run report.
///
/// Prepare-command environments are non-secret by contract. Secret-backed
/// environment names belong in `secret_environment`, and their values are
/// materialized only inside the container init process.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitPlan {
    /// Whether the runtime invocation wraps the logical workload with init.
    #[serde(default)]
    pub enabled: bool,
    /// Ordered argv-only setup commands.
    #[serde(default)]
    pub prepare: Vec<PrepareCommand>,
    /// Long-lived loopback/socket helpers started before the workload.
    #[serde(default)]
    pub services: Vec<InitServiceSpec>,
    /// Names populated from mounted secret files; values are never stored here.
    #[serde(default)]
    pub secret_environment: BTreeSet<String>,
}

/// How one host facility reaches the workload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardingTransport {
    #[default]
    Disabled,
    BindMount,
    AuthenticatedRelay,
}

/// Mount metadata retained without exposing a host source path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingMount {
    pub kind: PlannedMountKind,
    pub target: PathBuf,
    pub read_only: bool,
}

/// Serializable equivalent of the host runtime's mount kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedMountKind {
    Bind,
    Volume,
    Tmpfs,
}

/// Redacted description of host identity and tool configuration forwarding.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingMetadata {
    #[serde(default)]
    pub ssh_agent: ForwardingTransport,
    #[serde(default)]
    pub gpg_agent: ForwardingTransport,
    #[serde(default)]
    pub git_config: bool,
    #[serde(default)]
    pub known_hosts: bool,
    #[serde(default)]
    pub gh_config: bool,
    /// Names only. Exact non-secret values live in [`RuntimePlan::env`].
    #[serde(default)]
    pub environment_names: BTreeSet<String>,
    #[serde(default)]
    pub mounts: Vec<ForwardingMount>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Redacted context for the authenticated host-service boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostServiceMetadata {
    /// Resolved environment declarations that requested host access.
    #[serde(default)]
    pub declarations: Vec<HostServiceSpec>,
    /// Authorities added to general egress policy.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// Explicit exceptions to private-address blocking.
    #[serde(default)]
    pub allow_private: Vec<String>,
    /// Container-owned roots needed by Unix-socket bridges.
    #[serde(default)]
    pub ownership_paths: Vec<PathBuf>,
    /// Non-fatal omissions and policy explanations.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl HostServiceMetadata {
    /// Summarizes a prepared host-service plan without retaining token contents
    /// or host listener handles.
    #[must_use]
    pub fn from_prepared(declarations: Vec<HostServiceSpec>, plan: &HostServicePlan) -> Self {
        Self {
            declarations,
            allow_hosts: plan.allow_hosts.clone(),
            allow_private: plan.allow_private.clone(),
            ownership_paths: plan.ownership_paths.clone(),
            warnings: plan.warnings.clone(),
        }
    }
}

impl ForwardingMetadata {
    /// Produces a source-path-free summary while a prepared forwarding plan is
    /// still alive.
    #[must_use]
    pub fn from_prepared(plan: &ForwardingPlan) -> Self {
        let environment_names = plan.env.keys().cloned().collect::<BTreeSet<_>>();
        let ssh_agent = if plan.ssh_agent_relay.is_some() {
            ForwardingTransport::AuthenticatedRelay
        } else if environment_names.contains("SSH_AUTH_SOCK") {
            ForwardingTransport::BindMount
        } else {
            ForwardingTransport::Disabled
        };
        let gpg_agent = if plan.gpg_agent_relay.is_some() {
            ForwardingTransport::AuthenticatedRelay
        } else if environment_names.contains("GNUPGHOME") {
            ForwardingTransport::BindMount
        } else {
            ForwardingTransport::Disabled
        };
        let mounts = plan
            .mounts
            .iter()
            .map(|mount| ForwardingMount {
                kind: planned_mount_kind(mount.kind),
                target: mount.target.clone(),
                read_only: mount.read_only,
            })
            .collect();
        Self {
            ssh_agent,
            gpg_agent,
            git_config: environment_names.contains("GIT_CONFIG_GLOBAL"),
            known_hosts: environment_names.contains("CODEX_START_KNOWN_HOSTS"),
            gh_config: environment_names.contains("GH_CONFIG_DIR"),
            environment_names,
            mounts,
            warnings: plan.warnings.clone(),
        }
    }
}

/// Identity and logical-network facts that a raw [`RunRequest`] does not carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequestContext {
    pub project_id: String,
    pub environment: String,
    pub runtime: RuntimeKind,
    pub network: NetworkPlan,
    /// Absolute logical workdir used only when the request retains its image
    /// default and therefore has no engine workdir.
    pub fallback_workdir: PathBuf,
}

impl RunRequestContext {
    #[must_use]
    pub fn new(
        project_id: impl Into<String>,
        environment: impl Into<String>,
        runtime: RuntimeKind,
        network: NetworkPlan,
        fallback_workdir: PathBuf,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            environment: environment.into(),
            runtime,
            network,
            fallback_workdir,
        }
    }
}

impl HostLaunchPlan {
    /// Creates a launch from fully materialized logical and host-only parts.
    pub fn from_parts(
        container: ContainerPlan,
        runtime: RuntimePlan,
        init: InitPlan,
        forwarding: ForwardingMetadata,
        host_services: HostServiceMetadata,
    ) -> Result<Self, LaunchPlanError> {
        let plan = Self {
            schema_version: HOST_LAUNCH_PLAN_SCHEMA_VERSION,
            container,
            runtime,
            init,
            forwarding,
            host_services,
            session: SessionMetadata::default(),
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Captures an existing engine request exactly while reconstructing its
    /// runtime-neutral fields. Generated mount and port IDs are stable by index.
    pub fn from_run_request(
        request: RunRequest,
        context: RunRequestContext,
    ) -> Result<Self, LaunchPlanError> {
        let RunRequest {
            name,
            image,
            entrypoint,
            command,
            workdir,
            env,
            labels,
            mounts,
            publish,
            resources,
            network,
            network_alias,
            user_namespace,
            add_hosts,
            tty,
            interactive,
            detach,
            remove,
            read_only,
            drop_all_capabilities,
            add_capabilities,
            no_new_privileges,
            user,
            extra_args,
        } = request;

        let runtime_entrypoint = entrypoint.map(UnixArgument::from);
        let runtime_command = arguments_from_os(command);
        let logical_workdir = workdir
            .clone()
            .unwrap_or_else(|| context.fallback_workdir.clone());
        let mut container = ContainerPlan::new(
            context.project_id,
            context.environment,
            name,
            image,
            runtime_command.clone(),
            logical_workdir,
            context.network,
        );
        container.runtime = context.runtime;
        container.entrypoint = runtime_entrypoint.clone().map(|value| vec![value]);
        container.env = env
            .iter()
            .filter_map(|(name, value)| {
                value.to_str().map(|value| (name.clone(), value.to_owned()))
            })
            .collect();
        container.labels = labels;
        container.mounts = mounts
            .iter()
            .enumerate()
            .map(|(index, mount)| mount_to_core(index, mount))
            .collect::<Result<Vec<_>, _>>()?;
        container.ports = publish
            .iter()
            .enumerate()
            .map(|(index, port)| port_to_core(index, port))
            .collect::<Result<Vec<_>, _>>()?;
        container.resources = resources;
        container.tty = tty;
        container.stdin = interactive;
        container.remove = remove;
        container.read_only = read_only;
        container.cap_drop = if drop_all_capabilities {
            vec!["ALL".to_owned()]
        } else {
            Vec::new()
        };
        container.cap_add = add_capabilities;
        container.security_opt = if no_new_privileges {
            vec!["no-new-privileges".to_owned()]
        } else {
            Vec::new()
        };
        container.user = user;

        let runtime = RuntimePlan {
            entrypoint: runtime_entrypoint,
            command: runtime_command,
            workdir,
            env: env
                .into_iter()
                .map(|(name, value)| (name, UnixArgument::from(value)))
                .collect(),
            network,
            network_alias,
            user_namespace,
            add_hosts,
            detach,
            extra_args: arguments_from_os(extra_args),
        };
        Self::from_parts(
            container,
            runtime,
            InitPlan::default(),
            ForwardingMetadata::default(),
            HostServiceMetadata::default(),
        )
    }

    /// Checks core invariants and host-only consistency before any engine is
    /// contacted.
    pub fn validate(&self) -> Result<(), LaunchPlanError> {
        if self.schema_version != HOST_LAUNCH_PLAN_SCHEMA_VERSION {
            return Err(LaunchPlanError::UnsupportedSchema(self.schema_version));
        }
        self.container.validate()?;
        validate_runtime(self)?;
        validate_host_services(&self.host_services)?;
        validate_init(self)?;
        Ok(())
    }

    /// Converts this plan to the exact portable request consumed by the Docker
    /// or Podman adapter.
    pub fn to_run_request(&self) -> Result<RunRequest, LaunchPlanError> {
        self.validate()?;

        let mut extra_args = generated_runtime_args(&self.container);
        let entrypoint = match &self.runtime.entrypoint {
            Some(value) if value.to_str().is_some() => value.to_str().map(str::to_owned),
            Some(value) => {
                extra_args.extend([OsString::from("--entrypoint"), value.as_os_str().to_owned()]);
                None
            }
            None => None,
        };
        extra_args.extend(arguments_to_os(&self.runtime.extra_args));

        Ok(RunRequest {
            name: self.container.name.clone(),
            image: self.container.image.clone(),
            entrypoint,
            command: arguments_to_os(&self.runtime.command),
            workdir: self.runtime.workdir.clone(),
            env: self
                .runtime
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.as_os_str().to_owned()))
                .collect(),
            labels: self.container.labels.clone(),
            mounts: self.container.mounts.iter().map(mount_to_runtime).collect(),
            publish: self
                .container
                .ports
                .iter()
                .map(port_to_runtime)
                .collect::<Result<Vec<_>, _>>()?,
            resources: self.container.resources.clone(),
            network: self.runtime.network.clone(),
            network_alias: self.runtime.network_alias.clone(),
            user_namespace: self.runtime.user_namespace.clone(),
            add_hosts: self.runtime.add_hosts.clone(),
            tty: self.container.tty,
            interactive: self.container.stdin,
            detach: self.runtime.detach,
            remove: self.container.remove,
            read_only: self.container.read_only,
            drop_all_capabilities: self.container.cap_drop.iter().any(|cap| cap == "ALL"),
            add_capabilities: self.container.cap_add.clone(),
            no_new_privileges: self
                .container
                .security_opt
                .iter()
                .any(|option| option == "no-new-privileges"),
            user: self.container.user.clone(),
            extra_args,
        })
    }

    /// Produces the complete dry-run JSON after validation. Resolved secret
    /// values cannot appear because they are not accepted by this model.
    pub fn redacted_json(&self) -> Result<serde_json::Value, LaunchPlanError> {
        self.validate()?;
        serde_json::to_value(self).map_err(|error| LaunchPlanError::Serialize(error.to_string()))
    }
}

impl TryFrom<&HostLaunchPlan> for RunRequest {
    type Error = LaunchPlanError;

    fn try_from(value: &HostLaunchPlan) -> Result<Self, Self::Error> {
        value.to_run_request()
    }
}

fn validate_runtime(plan: &HostLaunchPlan) -> Result<(), LaunchPlanError> {
    let runtime = &plan.runtime;
    if runtime
        .entrypoint
        .as_ref()
        .is_some_and(argument_contains_nul)
        || runtime.command.iter().any(argument_contains_nul)
        || runtime.extra_args.iter().any(argument_contains_nul)
    {
        return Err(LaunchPlanError::Invalid(
            "runtime argv contains a NUL byte".to_owned(),
        ));
    }
    if let Some(workdir) = &runtime.workdir {
        if ContainerPath::new(workdir).is_err() || os_contains_nul(workdir.as_os_str()) {
            return Err(LaunchPlanError::Invalid(format!(
                "runtime workdir must be an absolute NUL-free path: {}",
                workdir.display()
            )));
        }
        if workdir != &plan.container.workdir {
            return Err(LaunchPlanError::Invalid(
                "logical and runtime workdirs disagree".to_owned(),
            ));
        }
    }
    validate_runtime_environment(plan)?;
    validate_runtime_network(plan)?;
    validate_resource_arg_conflicts(&plan.container.resources, &runtime.extra_args)?;
    validate_clean_map(&runtime.add_hosts, "add-host")?;
    if runtime
        .network_alias
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.contains('\0'))
    {
        return Err(LaunchPlanError::Invalid(
            "network alias must be non-empty and NUL-free".to_owned(),
        ));
    }
    if runtime
        .user_namespace
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.contains('\0'))
    {
        return Err(LaunchPlanError::Invalid(
            "user namespace must be non-empty and NUL-free".to_owned(),
        ));
    }
    for mount in &plan.container.mounts {
        if os_contains_nul(mount.target.as_os_str())
            || match &mount.source {
                MountSource::Bind { path } => os_contains_nul(path.as_os_str()),
                MountSource::Volume { name } => name.contains('\0'),
                MountSource::Tmpfs => false,
            }
        {
            return Err(LaunchPlanError::Invalid(format!(
                "mount {} contains a NUL byte",
                mount.id
            )));
        }
    }
    if plan.container.image.contains('\0')
        || plan
            .container
            .user
            .as_ref()
            .is_some_and(|value| value.contains('\0'))
        || plan
            .container
            .hostname
            .as_ref()
            .is_some_and(|value| value.contains('\0'))
        || plan
            .container
            .cap_drop
            .iter()
            .chain(&plan.container.cap_add)
            .chain(&plan.container.security_opt)
            .any(|value| value.contains('\0'))
    {
        return Err(LaunchPlanError::Invalid(
            "runtime text option contains a NUL byte".to_owned(),
        ));
    }
    if !plan.init.enabled {
        let (expected_entrypoint, expected_command) = flattened_invocation(&plan.container);
        if runtime.entrypoint != expected_entrypoint || runtime.command != expected_command {
            return Err(LaunchPlanError::Invalid(
                "runtime invocation differs from the workload without init enabled".to_owned(),
            ));
        }
    } else if runtime.entrypoint.is_none() && runtime.command.is_empty() {
        return Err(LaunchPlanError::Invalid(
            "enabled init requires a runtime wrapper invocation".to_owned(),
        ));
    }
    Ok(())
}

fn validate_resource_arg_conflicts(
    resources: &ResourceLimits,
    arguments: &[UnixArgument],
) -> Result<(), LaunchPlanError> {
    if resources.is_empty() {
        return Ok(());
    }
    for (index, argument) in arguments.iter().enumerate() {
        let Some(argument) = argument.to_str() else {
            continue;
        };
        let conflict = if resources.cpus.is_some()
            && ["--cpus", "--cpu-period", "--cpu-quota"]
                .iter()
                .any(|option| long_option(argument, option))
        {
            Some("cpus")
        } else if resources.cpu_shares.is_some()
            && (long_option(argument, "--cpu-shares") || short_option(argument, "-c"))
        {
            Some("cpu_shares")
        } else if resources.cpuset_cpus.is_some() && long_option(argument, "--cpuset-cpus") {
            Some("cpuset_cpus")
        } else if resources.memory.is_some()
            && (long_option(argument, "--memory") || short_option(argument, "-m"))
        {
            Some("memory")
        } else if resources.memory_reservation.is_some()
            && long_option(argument, "--memory-reservation")
        {
            Some("memory_reservation")
        } else if resources.memory_swap.is_some() && long_option(argument, "--memory-swap") {
            Some("memory_swap")
        } else if resources.pids_limit.is_some() && long_option(argument, "--pids-limit") {
            Some("pids_limit")
        } else if resources.shm_size.is_some() && long_option(argument, "--shm-size") {
            Some("shm_size")
        } else {
            conflicting_ulimit(resources, arguments, index, argument)
        };
        if let Some(key) = conflict {
            return Err(LaunchPlanError::Invalid(format!(
                "runtime argument `{argument}` conflicts with settings.resources.{key}"
            )));
        }
    }
    Ok(())
}

fn long_option(argument: &str, option: &str) -> bool {
    argument == option
        || argument
            .strip_prefix(option)
            .is_some_and(|suffix| suffix.starts_with('='))
}

fn short_option(argument: &str, option: &str) -> bool {
    argument == option
        || argument
            .strip_prefix(option)
            .is_some_and(|suffix| !suffix.is_empty() && !suffix.starts_with('-'))
}

fn conflicting_ulimit<'a>(
    resources: &'a ResourceLimits,
    arguments: &'a [UnixArgument],
    index: usize,
    argument: &'a str,
) -> Option<&'a str> {
    let value = argument.strip_prefix("--ulimit=").or_else(|| {
        (argument == "--ulimit")
            .then(|| arguments.get(index + 1).and_then(UnixArgument::to_str))
            .flatten()
    })?;
    let name = value.split_once('=').map_or(value, |(name, _)| name);
    resources.ulimits.contains_key(name).then_some(name)
}

fn validate_runtime_environment(plan: &HostLaunchPlan) -> Result<(), LaunchPlanError> {
    for (name, value) in &plan.runtime.env {
        if !valid_environment_name(name) || argument_contains_nul(value) {
            return Err(LaunchPlanError::Invalid(format!(
                "invalid runtime environment entry {name:?}"
            )));
        }
    }
    for (name, value) in &plan.container.env {
        let runtime_value = plan.runtime.env.get(name).ok_or_else(|| {
            LaunchPlanError::Invalid(format!(
                "runtime environment is missing logical variable {name:?}"
            ))
        })?;
        if runtime_value.to_str() != Some(value) {
            return Err(LaunchPlanError::Invalid(format!(
                "logical and runtime values disagree for environment variable {name:?}"
            )));
        }
    }
    let secret_names = plan
        .container
        .secrets
        .iter()
        .filter_map(|secret| secret.environment.as_deref())
        .chain(plan.init.secret_environment.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    if let Some(name) = secret_names.iter().find(|name| {
        plan.runtime.env.contains_key(**name) || plan.container.env.contains_key(**name)
    }) {
        return Err(LaunchPlanError::SecretValueInPlan((*name).to_owned()));
    }
    for command in &plan.init.prepare {
        if let Some(name) = command
            .env
            .keys()
            .find(|name| secret_names.contains(name.as_str()))
        {
            return Err(LaunchPlanError::SecretValueInPlan(name.clone()));
        }
    }
    Ok(())
}

fn validate_runtime_network(plan: &HostLaunchPlan) -> Result<(), LaunchPlanError> {
    let valid = match plan.container.network.mode {
        NetworkMode::Bridge => plan
            .runtime
            .network
            .as_deref()
            .is_none_or(|network| network == "bridge"),
        NetworkMode::Host => plan.runtime.network.as_deref() == Some("host"),
        NetworkMode::Offline | NetworkMode::Allowlist => {
            plan.runtime.network == plan.container.network.network_name
        }
    };
    if !valid {
        return Err(LaunchPlanError::Invalid(
            "logical and runtime networks disagree".to_owned(),
        ));
    }
    Ok(())
}

fn validate_init(plan: &HostLaunchPlan) -> Result<(), LaunchPlanError> {
    if !plan.init.enabled {
        if !plan.init.prepare.is_empty()
            || !plan.init.services.is_empty()
            || !plan.init.secret_environment.is_empty()
        {
            return Err(LaunchPlanError::Invalid(
                "init metadata requires init.enabled = true".to_owned(),
            ));
        }
        return Ok(());
    }

    let argv = workload_argv(&plan.container);
    let command = ExecSpec::from_argv(argv).map_err(|error| {
        LaunchPlanError::Invalid(format!("invalid init workload command: {error}"))
    })?;
    let spec = InitSpec {
        version: 1,
        account: None,
        uid: None,
        gid: None,
        cwd: Some(plan.container.workdir.clone()),
        clear_environment: false,
        env: BTreeMap::new(),
        secret_map: (!plan.init.secret_environment.is_empty())
            .then(|| PathBuf::from("/run/secrets/map.json")),
        secret_root: PathBuf::from("/run/secrets"),
        allow_insecure_secret_permissions: false,
        ownership_paths: Vec::new(),
        ssh: None,
        prepare: plan.init.prepare.clone(),
        services: plan.init.services.clone(),
        command,
    };
    codex_start_proxy::container_init::validate_spec(&spec)
        .map_err(|error| LaunchPlanError::Invalid(format!("invalid init metadata: {error}")))
}

fn validate_host_services(metadata: &HostServiceMetadata) -> Result<(), LaunchPlanError> {
    let mut ids = BTreeSet::new();
    for service in &metadata.declarations {
        if service.remove
            || !valid_runtime_name(&service.id)
            || !ids.insert(&service.id)
            || service.host.is_empty()
            || service.host.contains('\0')
            || service.port == 0
            || service.container_port == Some(0)
            || service
                .container_host
                .as_ref()
                .is_some_and(|host| host.is_empty() || host.contains('\0'))
        {
            return Err(LaunchPlanError::Invalid(format!(
                "invalid host service {:?}",
                service.id
            )));
        }
    }
    for (label, authorities) in [
        ("host-service allow", &metadata.allow_hosts),
        ("private host-service allow", &metadata.allow_private),
    ] {
        if let Some(authority) = authorities
            .iter()
            .find(|authority| authority.is_empty() || authority.contains('\0'))
        {
            return Err(LaunchPlanError::Invalid(format!(
                "invalid {label} authority {authority:?}"
            )));
        }
    }
    if let Some(path) = metadata
        .ownership_paths
        .iter()
        .find(|path| ContainerPath::new(path).is_err() || os_contains_nul(path.as_os_str()))
    {
        return Err(LaunchPlanError::Invalid(format!(
            "invalid host-service ownership path {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_clean_map(
    values: &BTreeMap<String, String>,
    label: &str,
) -> Result<(), LaunchPlanError> {
    if let Some((name, value)) = values.iter().find(|(name, value)| {
        name.is_empty() || name.contains('\0') || value.is_empty() || value.contains('\0')
    }) {
        return Err(LaunchPlanError::Invalid(format!(
            "invalid {label} mapping {name:?}={value:?}"
        )));
    }
    Ok(())
}

fn mount_to_core(index: usize, mount: &MountRequest) -> Result<MountPlan, LaunchPlanError> {
    let source = match mount.kind {
        RuntimeMountKind::Bind => MountSource::Bind {
            path: PathBuf::from(mount.source.clone().ok_or_else(|| {
                LaunchPlanError::Invalid(format!("bind mount {index} has no source"))
            })?),
        },
        RuntimeMountKind::Volume => {
            let source = mount.source.as_ref().ok_or_else(|| {
                LaunchPlanError::Invalid(format!("volume mount {index} has no source"))
            })?;
            let name = source.to_str().ok_or_else(|| {
                LaunchPlanError::Invalid(format!("volume mount {index} name is not UTF-8"))
            })?;
            MountSource::Volume {
                name: name.to_owned(),
            }
        }
        RuntimeMountKind::Tmpfs => {
            if mount.source.is_some() {
                return Err(LaunchPlanError::Invalid(format!(
                    "tmpfs mount {index} cannot have a source"
                )));
            }
            MountSource::Tmpfs
        }
    };
    Ok(MountPlan {
        id: format!("runtime-mount-{index}"),
        source,
        target: mount.target.clone(),
        read_only: mount.read_only,
    })
}

fn mount_to_runtime(mount: &MountPlan) -> MountRequest {
    let (kind, source) = match &mount.source {
        MountSource::Bind { path } => (RuntimeMountKind::Bind, Some(path.as_os_str().to_owned())),
        MountSource::Volume { name } => (RuntimeMountKind::Volume, Some(name.into())),
        MountSource::Tmpfs => (RuntimeMountKind::Tmpfs, None),
    };
    MountRequest {
        kind,
        source,
        target: mount.target.clone(),
        read_only: mount.read_only,
    }
}

fn port_to_core(index: usize, port: &PublishRequest) -> Result<PublishedPort, LaunchPlanError> {
    let protocol = match port.protocol.as_str() {
        "tcp" => PortProtocol::Tcp,
        "udp" => PortProtocol::Udp,
        other => {
            return Err(LaunchPlanError::Invalid(format!(
                "unsupported port protocol {other:?}"
            )));
        }
    };
    Ok(PublishedPort {
        id: format!("runtime-port-{index}"),
        host_ip: port.host_ip.to_string(),
        host_port: port.host_port,
        container_port: port.container_port,
        protocol,
    })
}

fn port_to_runtime(port: &PublishedPort) -> Result<PublishRequest, LaunchPlanError> {
    Ok(PublishRequest {
        host_ip: port
            .host_ip
            .parse::<IpAddr>()
            .map_err(|_| LaunchPlanError::Invalid(format!("invalid host IP {:?}", port.host_ip)))?,
        host_port: port.host_port,
        container_port: port.container_port,
        protocol: match port.protocol {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        }
        .to_owned(),
    })
}

fn generated_runtime_args(container: &ContainerPlan) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if let Some(hostname) = &container.hostname {
        arguments.extend([OsString::from("--hostname"), OsString::from(hostname)]);
    }
    for capability in container
        .cap_drop
        .iter()
        .filter(|capability| *capability != "ALL")
    {
        arguments.extend([OsString::from("--cap-drop"), OsString::from(capability)]);
    }
    for option in container
        .security_opt
        .iter()
        .filter(|option| *option != "no-new-privileges")
    {
        arguments.extend([OsString::from("--security-opt"), OsString::from(option)]);
    }
    arguments
}

fn flattened_invocation(container: &ContainerPlan) -> (Option<UnixArgument>, Vec<UnixArgument>) {
    let Some(entrypoint) = &container.entrypoint else {
        return (None, container.command.clone());
    };
    let mut entrypoint = entrypoint.iter().cloned();
    let program = entrypoint.next();
    let command = entrypoint
        .chain(container.command.iter().cloned())
        .collect();
    (program, command)
}

fn workload_argv(container: &ContainerPlan) -> Vec<OsString> {
    container
        .entrypoint
        .iter()
        .flatten()
        .chain(&container.command)
        .map(|argument| argument.as_os_str().to_owned())
        .collect()
}

fn arguments_from_os(values: Vec<OsString>) -> Vec<UnixArgument> {
    values.into_iter().map(UnixArgument::from).collect()
}

fn arguments_to_os(values: &[UnixArgument]) -> Vec<OsString> {
    values
        .iter()
        .map(|value| value.as_os_str().to_owned())
        .collect()
}

const fn planned_mount_kind(kind: RuntimeMountKind) -> PlannedMountKind {
    match kind {
        RuntimeMountKind::Bind => PlannedMountKind::Bind,
        RuntimeMountKind::Volume => PlannedMountKind::Volume,
        RuntimeMountKind::Tmpfs => PlannedMountKind::Tmpfs,
    }
}

fn argument_contains_nul(value: &UnixArgument) -> bool {
    value.as_os_str().as_encoded_bytes().contains(&0)
}

fn os_contains_nul(value: &OsStr) -> bool {
    value.as_encoded_bytes().contains(&0)
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
        })
}

fn valid_runtime_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && !value.starts_with(['-', '.'])
        && !value.ends_with(['-', '.'])
}

/// Validation or serialization failure for a host launch plan.
#[derive(Debug, Error)]
pub enum LaunchPlanError {
    #[error("unsupported host launch-plan schema {0}")]
    UnsupportedSchema(u32),
    #[error(transparent)]
    Container(#[from] ContainerPlanError),
    #[error("invalid host launch plan: {0}")]
    Invalid(String),
    #[error("secret-backed environment variable {0:?} was inserted into the serialized plan")]
    SecretValueInPlan(String),
    #[error("failed to serialize host launch plan: {0}")]
    Serialize(String),
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        net::{Ipv4Addr, SocketAddr},
        os::unix::ffi::OsStringExt,
        path::PathBuf,
    };

    use codex_start_core::{HostServiceSpec, NetworkPlan, RuntimeKind, SecretMount};
    use codex_start_proxy::container_init::{
        CommandSpec as PrepareCommand, InitServiceSpec, TcpForwardServiceSpec,
    };

    use super::{
        ForwardingMetadata, HostLaunchPlan, HostServiceMetadata, InitPlan, RunRequestContext,
    };
    use crate::runtime::{MountKind, MountRequest, PublishRequest, RunRequest};

    fn context(network: NetworkPlan) -> RunRequestContext {
        RunRequestContext::new(
            "project-one",
            "rust",
            RuntimeKind::Docker,
            network,
            PathBuf::from("/workspace"),
        )
    }

    fn complete_request() -> RunRequest {
        RunRequest {
            name: "codex-project".to_owned(),
            image: "example.invalid/codex:locked".to_owned(),
            entrypoint: Some("/usr/local/bin/codex-start-init".to_owned()),
            command: vec![
                OsString::from("run"),
                OsString::from_vec(vec![b'a', 0x80, b'b']),
            ],
            workdir: Some(PathBuf::from("/workspace")),
            env: BTreeMap::from([
                ("HOME".to_owned(), OsString::from("/home/codex")),
                ("NON_UTF8".to_owned(), OsString::from_vec(vec![b'x', 0xFF])),
            ]),
            labels: BTreeMap::from([("io.codex-start.managed".to_owned(), "true".to_owned())]),
            mounts: vec![MountRequest {
                kind: MountKind::Bind,
                source: Some(OsString::from("/host/workspace")),
                target: PathBuf::from("/workspace"),
                read_only: false,
            }],
            publish: vec![PublishRequest {
                host_ip: Ipv4Addr::LOCALHOST.into(),
                host_port: 5173,
                container_port: 5173,
                protocol: "tcp".to_owned(),
            }],
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
                nofile = "65536:65536"
                "#,
            )
            .expect("resource limits"),
            network: Some("isolated-one".to_owned()),
            network_alias: Some("workload".to_owned()),
            user_namespace: Some("keep-id".to_owned()),
            add_hosts: BTreeMap::from([(
                "host.docker.internal".to_owned(),
                "host-gateway".to_owned(),
            )]),
            tty: true,
            interactive: true,
            detach: false,
            remove: true,
            read_only: true,
            drop_all_capabilities: true,
            add_capabilities: vec!["NET_BIND_SERVICE".to_owned()],
            no_new_privileges: true,
            user: Some("1000:1000".to_owned()),
            extra_args: vec![
                OsString::from("--annotation"),
                OsString::from_vec(vec![0xFE, b'z']),
            ],
        }
    }

    #[test]
    fn run_request_roundtrip_preserves_every_engine_field_and_non_utf8_argv() {
        let request = complete_request();
        let plan = HostLaunchPlan::from_run_request(
            request.clone(),
            context(NetworkPlan::offline("isolated-one".to_owned())),
        )
        .expect("capture request");
        assert_eq!(plan.to_run_request().expect("restore request"), request);

        let json = serde_json::to_vec(&plan).expect("serialize plan");
        let restored: HostLaunchPlan = serde_json::from_slice(&json).expect("deserialize plan");
        assert_eq!(restored, plan);
        assert_eq!(restored.to_run_request().expect("restore request"), request);
    }

    #[test]
    fn typed_resources_reject_conflicting_expert_runtime_arguments() {
        for arguments in [
            vec![OsString::from("--cpus=4")],
            vec![OsString::from("--cpu-period"), OsString::from("100000")],
            vec![OsString::from("-c2048")],
            vec![OsString::from("--cpuset-cpus=4-7")],
            vec![OsString::from("--memory"), OsString::from("12g")],
            vec![OsString::from("-m12g")],
            vec![OsString::from("--memory-reservation=6g")],
            vec![OsString::from("--memory-swap=-1")],
            vec![OsString::from("--pids-limit=1024")],
            vec![OsString::from("--shm-size=2g")],
            vec![
                OsString::from("--ulimit"),
                OsString::from("nofile=1024:1024"),
            ],
        ] {
            let mut request = complete_request();
            request.extra_args = arguments;
            let error = HostLaunchPlan::from_run_request(
                request,
                context(NetworkPlan::offline("isolated-one".to_owned())),
            )
            .expect_err("typed resource conflict");
            assert!(error.to_string().contains("settings.resources"));
        }

        let mut non_conflicting = complete_request();
        non_conflicting.extra_args = vec![
            OsString::from("--ulimit=stack=8192:8192"),
            OsString::from("--memory-swappiness=20"),
        ];
        HostLaunchPlan::from_run_request(
            non_conflicting,
            context(NetworkPlan::offline("isolated-one".to_owned())),
        )
        .expect("non-conflicting expert resource options");
    }

    #[test]
    fn dry_run_contains_init_forwarding_and_host_service_metadata() {
        let request = complete_request();
        let mut plan = HostLaunchPlan::from_run_request(
            request,
            context(NetworkPlan::offline("isolated-one".to_owned())),
        )
        .expect("capture request");
        plan.init = InitPlan {
            enabled: true,
            prepare: vec![PrepareCommand {
                program: "cargo".to_owned(),
                args: vec!["fetch".to_owned()],
                env: BTreeMap::new(),
                cwd: Some(PathBuf::from("/workspace")),
            }],
            services: vec![InitServiceSpec::TcpForward(TcpForwardServiceSpec {
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 11434)),
                target: "host.docker.internal:11434".to_owned(),
                max_connections: 16,
                connect_timeout_seconds: 5,
                idle_timeout_seconds: 60,
            })],
            secret_environment: BTreeSet::from(["OPENAI_API_KEY".to_owned()]),
        };
        plan.forwarding = ForwardingMetadata {
            git_config: true,
            known_hosts: true,
            environment_names: BTreeSet::from(["GIT_CONFIG_GLOBAL".to_owned()]),
            ..ForwardingMetadata::default()
        };
        plan.host_services = HostServiceMetadata {
            declarations: vec![HostServiceSpec {
                id: "ollama".to_owned(),
                host: "host.docker.internal".to_owned(),
                port: 11434,
                container_host: Some("127.0.0.1".to_owned()),
                container_port: Some(11434),
                allow_private: true,
                remove: false,
            }],
            allow_hosts: vec!["host.docker.internal:11434".to_owned()],
            allow_private: vec!["host.docker.internal:11434".to_owned()],
            ownership_paths: Vec::new(),
            warnings: Vec::new(),
        };
        plan.container.secrets.push(SecretMount {
            name: "openai".to_owned(),
            provider: "keychain-openai".to_owned(),
            path: PathBuf::from("/run/secrets/openai"),
            environment: Some("OPENAI_API_KEY".to_owned()),
        });

        let json = plan.redacted_json().expect("redacted plan");
        let text = serde_json::to_string(&json).expect("json text");
        assert!(text.contains("cargo"));
        assert!(text.contains("tcp-forward"));
        assert!(text.contains("GIT_CONFIG_GLOBAL"));
        assert!(text.contains("ollama"));
        assert!(text.contains("keychain-openai"));
        assert!(!text.contains("super-secret-value"));
    }

    #[test]
    fn resolved_secret_environment_values_are_rejected_before_dry_run() {
        let request = complete_request();
        let mut plan = HostLaunchPlan::from_run_request(
            request,
            context(NetworkPlan::offline("isolated-one".to_owned())),
        )
        .expect("capture request");
        plan.init.enabled = true;
        plan.init
            .secret_environment
            .insert("OPENAI_API_KEY".to_owned());
        plan.runtime
            .env
            .insert("OPENAI_API_KEY".to_owned(), "super-secret-value".into());
        assert!(plan.redacted_json().is_err());
    }

    #[test]
    fn core_validation_rejects_conflicting_mounts_and_host_network_ports() {
        let mut request = complete_request();
        request.mounts.push(MountRequest {
            kind: MountKind::Tmpfs,
            source: None,
            target: PathBuf::from("/workspace"),
            read_only: false,
        });
        assert!(
            HostLaunchPlan::from_run_request(
                request,
                context(NetworkPlan::offline("isolated-one".to_owned())),
            )
            .is_err()
        );

        let mut request = complete_request();
        request.network = Some("host".to_owned());
        assert!(HostLaunchPlan::from_run_request(request, context(NetworkPlan::host())).is_err());
    }
}
