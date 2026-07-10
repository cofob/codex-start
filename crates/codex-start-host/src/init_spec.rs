//! Versioned workload-init specification materialization.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use codex_start_proxy::container_init::{
    CommandSpec, ExecSpec, InitServiceSpec, InitSpec, SshSetup,
};
use tempfile::TempDir;

use crate::{
    error::{HostError, Result},
    paths::set_private_file,
    runtime::{MountKind, MountRequest},
};

/// Private host file containing a validated init specification.
#[derive(Debug)]
pub struct InitBundle {
    directory: TempDir,
}

/// Complete, already-planned input for one container init specification.
#[derive(Debug)]
pub struct InitBundleOptions {
    pub identity: WorkloadIdentity,
    pub cwd: PathBuf,
    pub prepare: Vec<CommandSpec>,
    pub command: Vec<OsString>,
    pub account: Option<String>,
    pub secret_map: Option<PathBuf>,
    pub ownership_paths: Vec<PathBuf>,
    pub services: Vec<InitServiceSpec>,
    pub ssh: Option<SshSetup>,
}

/// Numeric identity used by initialization and the final workload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadIdentity {
    uid: u32,
    gid: u32,
}

impl WorkloadIdentity {
    /// Resolve the platform identity represented inside environment images.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unnecessary_wraps))]
    pub fn detect() -> Result<Self> {
        #[cfg(target_os = "linux")]
        let (uid, gid) = host_identity()?;
        #[cfg(not(target_os = "linux"))]
        let (uid, gid) = host_identity();
        Ok(Self { uid, gid })
    }

    /// Target workload UID.
    pub const fn uid(self) -> u32 {
        self.uid
    }

    /// Target workload GID.
    pub const fn gid(self) -> u32 {
        self.gid
    }
}

impl InitBundle {
    /// Create the exact spec consumed by `codex-start-init run`.
    pub fn create(runtime_parent: &Path, options: InitBundleOptions) -> Result<Self> {
        let command = ExecSpec::from_argv(options.command)
            .map_err(|error| HostError::Config(format!("invalid container command: {error}")))?;
        let directory = tempfile::Builder::new()
            .prefix("init-")
            .tempdir_in(runtime_parent)
            .map_err(|source| HostError::io(runtime_parent, source))?;
        let uid = options.identity.uid();
        let gid = options.identity.gid();
        let spec = InitSpec {
            version: 1,
            uid: Some(uid),
            gid: Some(gid),
            account: options.account,
            cwd: Some(options.cwd),
            clear_environment: false,
            env: BTreeMap::new(),
            secret_map: options.secret_map,
            secret_root: PathBuf::from("/run/secrets"),
            allow_insecure_secret_permissions: false,
            ssh: options.ssh,
            ownership_paths: options.ownership_paths,
            services: options.services,
            prepare: options.prepare,
            command,
        };
        codex_start_proxy::container_init::validate_spec(&spec)
            .map_err(|error| HostError::Config(format!("invalid generated init spec: {error}")))?;
        let path = directory.path().join("spec.json");
        let json = serde_json::to_vec_pretty(&spec)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        fs::write(&path, json).map_err(|source| HostError::io(&path, source))?;
        set_private_file(&path)?;
        Ok(Self { directory })
    }

    /// Read-only bind mount for the init specification.
    pub fn mount(&self) -> MountRequest {
        MountRequest {
            kind: MountKind::Bind,
            source: Some(self.directory.path().as_os_str().to_owned()),
            target: PathBuf::from("/run/codex-start/init"),
            read_only: true,
        }
    }

    /// Container path supplied to the init binary.
    pub const fn container_path() -> &'static str {
        "/run/codex-start/init/spec.json"
    }
}

#[cfg(target_os = "linux")]
fn host_identity() -> Result<(u32, u32)> {
    let uid = command_number("-u")?;
    let gid = command_number("-g")?;
    // A root-owned host checkout must not silently turn the workload into a
    // root process. Shipped images reserve 1000:1000 for the `codex` user.
    Ok(if uid == 0 { (1000, 1000) } else { (uid, gid) })
}

#[cfg(not(target_os = "linux"))]
const fn host_identity() -> (u32, u32) {
    (1000, 1000)
}

#[cfg(target_os = "linux")]
fn command_number(flag: &str) -> Result<u32> {
    let output = std::process::Command::new("id")
        .arg(flag)
        .output()
        .map_err(|source| HostError::CommandIo {
            program: OsString::from("id"),
            source,
        })?;
    if !output.status.success() {
        return Err(HostError::Runtime(format!(
            "id {flag} failed with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| HostError::Runtime(format!("id {flag} returned an invalid number")))
}
