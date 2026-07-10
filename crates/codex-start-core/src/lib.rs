//! Shared domain types and configuration resolution for `codex-start`.
//!
//! This crate deliberately contains no container-engine, Git, or process execution
//! code.  It turns trusted inputs into validated, runtime-neutral descriptions that
//! the host adapter can execute.

#![forbid(unsafe_code)]
// These domain records intentionally mirror independent runtime switches, and
// errors are self-describing enums used directly by CLI diagnostics.
#![allow(clippy::missing_errors_doc, clippy::struct_excessive_bools)]

pub mod config;
pub mod environment;
pub mod plan;
pub mod project;

pub use config::{
    CodexConfig, ConfigDocument, ConfigError, ConfigLayer, ConfigLayerKind, ConfigPatch,
    ConfigResolver, EffectiveConfig, ForwardingConfig, ForwardingPatch, GitConfig, GitPatch,
    HomeConfig, HomeKind, McpOauthCallback, NetworkMode, ProfileConfig, Provenance, ProxyConfig,
    ProxyPatch, ResolvedConfig, RuntimeKind, SecretProvider, SecretProviderKind, SshAgentBridge,
    TtyMode, ValueSource, WorktreeMode, environment_patch,
};
pub use environment::{
    BuildSpec, CacheScope, CacheSpec, CommandSpec, EnvironmentError, EnvironmentManifest,
    EnvironmentRegistry, HostServiceSpec, ManifestSource, MountKind, MountSpec, PortProtocol,
    PortSpec, ResolvedEnvironment,
};
pub use plan::{
    ContainerPlan, ContainerPlanError, LifecycleError, LifecycleState, ManagedResource, MountPlan,
    MountSource, NetworkPlan, ProxyPlan, PublishedPort, ResourceKind, SecretMount, SidecarPlan,
    UnixArgument,
};
pub use project::{
    AppPaths, ProjectError, ProjectIdentity, ProjectKind, canonical_path_hash,
    sanitize_branch_component, sanitize_resource_name,
};
