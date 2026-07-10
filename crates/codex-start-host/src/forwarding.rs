//! Host identity, signing, and tool configuration forwarding.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use codex_start_core::SshAgentBridge;
use tempfile::TempDir;

use crate::{
    command::{CommandSpec, run_capture, run_interactive},
    error::{HostError, Result},
    paths::{create_private_dir, set_private_file},
    runtime::{MountKind, MountRequest, Runtime, RuntimeKind},
};

const CONTAINER_SSH_WRAPPER: &str = "/usr/local/bin/ssh";

/// Independent forwarding switches.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ForwardingOptions {
    pub ssh_agent: bool,
    pub ssh_agent_bridge: SshAgentBridge,
    pub gpg_agent: bool,
    pub git_config: bool,
    pub known_hosts: bool,
    pub gh_config: bool,
    pub git_config_file: Option<PathBuf>,
    pub known_hosts_file: Option<PathBuf>,
    pub container_ssh_dir: PathBuf,
    pub ssh_user: Option<String>,
}

impl Default for ForwardingOptions {
    fn default() -> Self {
        Self {
            ssh_agent: true,
            ssh_agent_bridge: SshAgentBridge::Auto,
            gpg_agent: true,
            git_config: true,
            known_hosts: true,
            gh_config: true,
            git_config_file: None,
            known_hosts_file: None,
            container_ssh_dir: PathBuf::from("/home/codex/.ssh"),
            ssh_user: None,
        }
    }
}

/// Prepared mounts and environment kept alive for a container run.
#[derive(Debug)]
pub struct ForwardingPlan {
    /// Read-only or read-write host mounts.
    pub mounts: Vec<MountRequest>,
    /// Non-secret child environment.
    pub env: BTreeMap<String, std::ffi::OsString>,
    /// Informational warnings that do not prevent launch.
    pub warnings: Vec<String>,
    /// Host SSH-agent socket requiring an authenticated TCP fallback.
    pub ssh_agent_relay: Option<PathBuf>,
    /// Host GPG-agent socket requiring an authenticated TCP fallback.
    pub gpg_agent_relay: Option<PathBuf>,
    _temporary: Option<TempDir>,
    _started_ssh_agent: Option<PreparedSshAgent>,
}

impl ForwardingPlan {
    /// Inspect host facilities and construct a portable forwarding plan.
    pub fn prepare(
        runtime: &Runtime,
        options: &ForwardingOptions,
        runtime_parent: &Path,
    ) -> Result<Self> {
        create_private_dir(runtime_parent)?;
        let temporary = tempfile::Builder::new()
            .prefix("forwarding-")
            .tempdir_in(runtime_parent)
            .map_err(|source| HostError::io(runtime_parent, source))?;
        let mut mounts = Vec::new();
        let mut env_map = BTreeMap::new();
        let mut warnings = Vec::new();
        let home = env::var_os("HOME").map(PathBuf::from);

        let started_ssh_agent = if options.ssh_agent {
            ensure_host_ssh_agent(&mut warnings)?
        } else {
            None
        };
        let ssh_agent_relay = options
            .ssh_agent
            .then(|| {
                prepare_ssh_agent(
                    runtime,
                    options.ssh_agent_bridge,
                    started_ssh_agent
                        .as_ref()
                        .map(|agent| agent.socket.as_path()),
                    &mut mounts,
                    &mut env_map,
                    &mut warnings,
                )
            })
            .flatten();
        let gpg_agent_relay = if options.gpg_agent {
            prepare_gpg(runtime, &mut mounts, &mut env_map, &mut warnings)?
        } else {
            None
        };
        if options.git_config {
            if let Some(source) = configured_host_path(
                options.git_config_file.as_deref(),
                home.as_deref(),
                ".gitconfig",
            )? {
                prepare_git_config(
                    home.as_deref(),
                    &source,
                    temporary.path(),
                    &mut mounts,
                    &mut env_map,
                    &mut warnings,
                )?;
            }
        }
        prepare_known_hosts(
            options,
            home.as_deref(),
            temporary.path(),
            &mut mounts,
            &mut env_map,
            &mut warnings,
        )?;
        prepare_ssh_user(options, temporary.path(), &mut mounts, &mut env_map)?;
        enable_git_ssh_wrapper(&mut env_map);
        prepare_gh_config(options, home.as_deref(), &mut mounts, &mut env_map);

        Ok(Self {
            mounts,
            env: env_map,
            warnings,
            ssh_agent_relay,
            gpg_agent_relay,
            _temporary: Some(temporary),
            _started_ssh_agent: started_ssh_agent,
        })
    }
}

fn prepare_known_hosts(
    options: &ForwardingOptions,
    home: Option<&Path>,
    temporary: &Path,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !options.known_hosts {
        return Ok(());
    }
    let Some(known_hosts) = configured_host_path(
        options.known_hosts_file.as_deref(),
        home,
        ".ssh/known_hosts",
    )?
    else {
        return Ok(());
    };
    if !known_hosts.is_file() {
        warnings.push(format!("known_hosts not found: {}", known_hosts.display()));
        return Ok(());
    }
    let copy = temporary.join("known_hosts");
    fs::copy(&known_hosts, &copy).map_err(|source| HostError::io(&known_hosts, source))?;
    set_private_file(&copy)?;
    let target = options.container_ssh_dir.join("known_hosts");
    mounts.push(bind(&copy, &target, true));
    env_map.insert(
        "CODEX_START_KNOWN_HOSTS".to_owned(),
        target.into_os_string(),
    );
    Ok(())
}

fn enable_git_ssh_wrapper(env_map: &mut BTreeMap<String, std::ffi::OsString>) {
    if env_map.contains_key("CODEX_START_SSH_CONFIG")
        || env_map.contains_key("CODEX_START_KNOWN_HOSTS")
    {
        env_map.insert("GIT_SSH".to_owned(), CONTAINER_SSH_WRAPPER.into());
        env_map.insert("GIT_SSH_VARIANT".to_owned(), "ssh".into());
    }
}

fn prepare_ssh_user(
    options: &ForwardingOptions,
    temporary: &Path,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
) -> Result<()> {
    let Some(user) = options.ssh_user.clone().or_else(|| env::var("USER").ok()) else {
        return Ok(());
    };
    if !valid_ssh_user(&user) {
        return Err(HostError::Config(
            "forwarding SSH user contains unsupported characters".to_owned(),
        ));
    }
    let config = temporary.join("ssh-config");
    fs::write(&config, format!("Host *\n  User {user}\n"))
        .map_err(|source| HostError::io(&config, source))?;
    set_private_file(&config)?;
    mounts.push(bind(
        &config,
        options.container_ssh_dir.join("config"),
        true,
    ));
    env_map.insert(
        "CODEX_START_SSH_CONFIG".to_owned(),
        options.container_ssh_dir.join("config").into_os_string(),
    );
    env_map.insert("CODEX_START_SSH_USER".to_owned(), user.into());
    Ok(())
}

fn prepare_gh_config(
    options: &ForwardingOptions,
    home: Option<&Path>,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
) {
    let Some(home) = home.filter(|_| options.gh_config) else {
        return;
    };
    let gh = home.join(".config/gh");
    if gh.is_dir() {
        mounts.push(bind(&gh, "/home/codex/.config/gh", false));
        env_map.insert("GH_CONFIG_DIR".to_owned(), "/home/codex/.config/gh".into());
    }
}

#[derive(Debug)]
struct PreparedSshAgent {
    socket: PathBuf,
    pid: Option<String>,
}

impl Drop for PreparedSshAgent {
    fn drop(&mut self) {
        let Some(pid) = &self.pid else {
            return;
        };
        let mut command = CommandSpec::new("ssh-agent").arg("-k");
        command
            .env
            .insert("SSH_AUTH_SOCK".into(), self.socket.as_os_str().to_owned());
        command.env.insert("SSH_AGENT_PID".into(), pid.into());
        if let Err(error) = run_capture(&command) {
            tracing::warn!(%error, "could not stop codex-start SSH agent");
        }
    }
}

fn ensure_host_ssh_agent(warnings: &mut Vec<String>) -> Result<Option<PreparedSshAgent>> {
    if let Some(socket) = env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)
        && is_socket(&socket)
    {
        ensure_ssh_identities(&socket, None, warnings)?;
        return Ok(Some(PreparedSshAgent { socket, pid: None }));
    }
    let output = match run_capture(&CommandSpec::new("ssh-agent").arg("-s")) {
        Ok(output) => output,
        Err(HostError::CommandIo { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            warnings.push("ssh-agent is unavailable; SSH-agent forwarding is disabled".to_owned());
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !output.status.success() {
        warnings
            .push("ssh-agent could not be started; SSH-agent forwarding is disabled".to_owned());
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let socket = parse_agent_assignment(&text, "SSH_AUTH_SOCK").map(PathBuf::from);
    let pid = parse_agent_assignment(&text, "SSH_AGENT_PID");
    let Some(socket) = socket.filter(|path| is_socket(path)) else {
        warnings.push("ssh-agent did not report a usable socket".to_owned());
        return Ok(None);
    };
    let agent = PreparedSshAgent { socket, pid };
    ensure_ssh_identities(&agent.socket, agent.pid.as_deref(), warnings)?;
    Ok(Some(agent))
}

fn parse_agent_assignment(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .and_then(|value| value.split(';').next())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn ensure_ssh_identities(
    socket: &Path,
    pid: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut list = CommandSpec::new("ssh-add").arg("-l");
    set_agent_environment(&mut list, socket, pid);
    let listed = match run_capture(&list) {
        Ok(output) => output.status.success(),
        Err(HostError::CommandIo { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            warnings.push("ssh-add is unavailable; the agent may contain no identities".to_owned());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if listed {
        return Ok(());
    }
    let mut add = CommandSpec::new("ssh-add").arg("-q");
    set_agent_environment(&mut add, socket, pid);
    if run_interactive(&add)? != 0 {
        warnings.push("ssh-add failed or no default key was added".to_owned());
    }
    Ok(())
}

fn set_agent_environment(spec: &mut CommandSpec, socket: &Path, pid: Option<&str>) {
    spec.env
        .insert("SSH_AUTH_SOCK".into(), socket.as_os_str().to_owned());
    if let Some(pid) = pid {
        spec.env.insert("SSH_AGENT_PID".into(), pid.into());
    }
}

fn prepare_ssh_agent(
    runtime: &Runtime,
    bridge: SshAgentBridge,
    agent_socket: Option<&Path>,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    let relay_requested = bridge == SshAgentBridge::Tcp
        || cfg!(target_os = "macos")
            && runtime.kind() == RuntimeKind::Podman
            && bridge == SshAgentBridge::Auto;
    if relay_requested {
        return agent_socket.map(Path::to_path_buf).or_else(|| {
            warnings.push("SSH agent socket is unavailable; forwarding is disabled".to_owned());
            None
        });
    }

    #[cfg(target_os = "macos")]
    if runtime.kind() == RuntimeKind::Docker {
        let desktop_socket = Path::new("/run/host-services/ssh-auth.sock");
        mounts.push(bind(
            desktop_socket,
            "/run/host-services/ssh-auth.sock",
            false,
        ));
        env_map.insert(
            "SSH_AUTH_SOCK".to_owned(),
            "/run/host-services/ssh-auth.sock".into(),
        );
        return None;
    }

    if let Some(socket) = agent_socket {
        if is_socket(socket) {
            mounts.push(bind(socket, "/run/codex-start/ssh-agent.sock", false));
            env_map.insert(
                "SSH_AUTH_SOCK".to_owned(),
                "/run/codex-start/ssh-agent.sock".into(),
            );
            return None;
        }
        warnings.push(format!(
            "SSH_AUTH_SOCK is not a socket: {}",
            socket.display()
        ));
    } else {
        warnings.push("SSH_AUTH_SOCK is unset; SSH-agent forwarding is disabled".to_owned());
    }
    None
}

fn prepare_gpg(
    runtime: &Runtime,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
    warnings: &mut Vec<String>,
) -> Result<Option<PathBuf>> {
    let launch = match run_capture(&CommandSpec::new("gpgconf").args(["--launch", "gpg-agent"])) {
        Ok(output) => output,
        Err(HostError::CommandIo { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            warnings.push("gpgconf is unavailable; GPG-agent forwarding is disabled".to_owned());
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !launch.status.success() {
        warnings.push("gpgconf could not launch gpg-agent; probing existing sockets".to_owned());
    }
    let output =
        match run_capture(&CommandSpec::new("gpgconf").args(["--list-dirs", "agent-extra-socket"]))
        {
            Ok(output) => output,
            Err(HostError::CommandIo { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                warnings
                    .push("gpgconf is unavailable; GPG-agent forwarding is disabled".to_owned());
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    let mut socket = output
        .status
        .success()
        .then(|| PathBuf::from(output.stdout_text()));
    if socket.as_ref().is_none_or(|path| !is_socket(path)) {
        let fallback =
            run_capture(&CommandSpec::new("gpgconf").args(["--list-dirs", "agent-socket"]))?;
        socket = fallback
            .status
            .success()
            .then(|| PathBuf::from(fallback.stdout_text()));
    }
    if let Some(socket) = socket.filter(|path| is_socket(path)) {
        env_map.insert("GNUPGHOME".to_owned(), "/home/codex/.gnupg".into());
        env_map.insert("GPG_TTY".to_owned(), "/dev/tty".into());
        if cfg!(target_os = "macos") || runtime.kind() == RuntimeKind::Podman {
            return Ok(Some(socket));
        }
        mounts.push(bind(&socket, "/home/codex/.gnupg/S.gpg-agent", false));
    } else {
        warnings.push("GPG agent socket was not found".to_owned());
    }
    Ok(None)
}

fn prepare_git_config(
    home: Option<&Path>,
    source: &Path,
    temporary: &Path,
    mounts: &mut Vec<MountRequest>,
    env_map: &mut BTreeMap<String, std::ffi::OsString>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if !source.is_file() {
        warnings.push(format!("Git configuration not found: {}", source.display()));
        return Ok(());
    }
    let copy = temporary.join("gitconfig");
    fs::copy(source, &copy).map_err(|error| HostError::io(&copy, error))?;
    set_private_file(&copy)?;
    mounts.push(bind(&copy, "/home/codex/.gitconfig", true));
    env_map.insert(
        "GIT_CONFIG_GLOBAL".to_owned(),
        "/home/codex/.gitconfig".into(),
    );

    for (key, default_target) in [
        ("user.signingkey", None),
        (
            "gpg.ssh.allowedSignersFile",
            Some("/home/codex/.gitallowedsigners"),
        ),
    ] {
        let output = run_capture(&CommandSpec::new("git").args([
            "config",
            "--file",
            source.to_string_lossy().as_ref(),
            "--get-all",
            key,
        ]))?;
        if !output.status.success() {
            continue;
        }
        for value in output.stdout_text().lines() {
            let host_path = home.and_then(|home| expand_git_path(home, value));
            let Some(host_path) = host_path.filter(|path| path.is_file()) else {
                continue;
            };
            let target = default_target.map_or_else(
                || container_git_path(value),
                |target| Some(PathBuf::from(target)),
            );
            if let Some(target) = target {
                if !mounts.iter().any(|mount| mount.target == target) {
                    mounts.push(bind(&host_path, &target, true));
                }
            }
        }
    }
    let default_signers = home.map(|home| home.join(".gitallowedsigners"));
    if default_signers.as_ref().is_some_and(|path| path.is_file())
        && !mounts
            .iter()
            .any(|mount| mount.target == Path::new("/home/codex/.gitallowedsigners"))
    {
        mounts.push(bind(
            default_signers.as_ref().expect("checked above"),
            "/home/codex/.gitallowedsigners",
            true,
        ));
    }
    Ok(())
}

fn valid_ssh_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 255
        && user.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'+' | b'-')
        })
}

fn configured_host_path(
    configured: Option<&Path>,
    home: Option<&Path>,
    default_relative: &str,
) -> Result<Option<PathBuf>> {
    match configured {
        Some(path) if path == Path::new("~") => {
            home.map(Path::to_path_buf).map(Some).ok_or_else(|| {
                HostError::Config(
                    "HOME is unset, so a ~ forwarding path cannot be expanded".to_owned(),
                )
            })
        }
        Some(path) if path.starts_with("~/") => {
            let relative = path.strip_prefix("~/").expect("prefix checked");
            home.map(|home| Some(home.join(relative))).ok_or_else(|| {
                HostError::Config(
                    "HOME is unset, so a ~/ forwarding path cannot be expanded".to_owned(),
                )
            })
        }
        Some(path) => Ok(Some(path.to_path_buf())),
        None => Ok(home.map(|home| home.join(default_relative))),
    }
}

fn expand_git_path(home: &Path, value: &str) -> Option<PathBuf> {
    if value == "~" {
        Some(home.to_path_buf())
    } else if let Some(relative) = value.strip_prefix("~/") {
        Some(home.join(relative))
    } else {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    }
}

fn container_git_path(value: &str) -> Option<PathBuf> {
    if value == "~" {
        Some(PathBuf::from("/home/codex"))
    } else if let Some(relative) = value.strip_prefix("~/") {
        Some(Path::new("/home/codex").join(relative))
    } else {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    }
}

fn bind(source: &Path, target: impl AsRef<Path>, read_only: bool) -> MountRequest {
    MountRequest {
        kind: MountKind::Bind,
        source: Some(source.as_os_str().to_owned()),
        target: target.as_ref().to_path_buf(),
        read_only,
    }
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs,
        path::{Path, PathBuf},
    };

    use super::{
        ForwardingOptions, container_git_path, enable_git_ssh_wrapper, expand_git_path,
        prepare_known_hosts, prepare_ssh_user,
    };

    #[test]
    fn rewrites_home_relative_git_paths() {
        assert_eq!(
            expand_git_path(Path::new("/Users/test"), "~/.ssh/signing.pub"),
            Some(PathBuf::from("/Users/test/.ssh/signing.pub"))
        );
        assert_eq!(
            container_git_path("~/.ssh/signing.pub"),
            Some(PathBuf::from("/home/codex/.ssh/signing.pub"))
        );
        assert_eq!(container_git_path("relative"), None);
    }

    #[test]
    fn custom_container_ssh_directory_drives_mounts_and_git_wrapper() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("known hosts");
        fs::write(&source, "example.test ssh-ed25519 AAAA\n").expect("known hosts");
        let options = ForwardingOptions {
            known_hosts_file: Some(source),
            container_ssh_dir: PathBuf::from("/home/codex/custom ssh"),
            ssh_user: Some("git-user".to_owned()),
            ..ForwardingOptions::default()
        };
        let temporary = root.path().join("temporary");
        fs::create_dir(&temporary).expect("temporary");
        let mut mounts = Vec::new();
        let mut env = BTreeMap::new();
        let mut warnings = Vec::new();

        prepare_known_hosts(
            &options,
            None,
            &temporary,
            &mut mounts,
            &mut env,
            &mut warnings,
        )
        .expect("known hosts");
        prepare_ssh_user(&options, &temporary, &mut mounts, &mut env).expect("SSH user");
        enable_git_ssh_wrapper(&mut env);

        assert!(warnings.is_empty());
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/home/codex/custom ssh/known_hosts") && mount.read_only
        }));
        assert!(mounts.iter().any(|mount| {
            mount.target == Path::new("/home/codex/custom ssh/config") && mount.read_only
        }));
        assert_eq!(
            env.get("GIT_SSH").map(std::ffi::OsString::as_os_str),
            Some(OsStr::new("/usr/local/bin/ssh"))
        );
        assert_eq!(
            env.get("GIT_SSH_VARIANT")
                .map(std::ffi::OsString::as_os_str),
            Some(OsStr::new("ssh"))
        );
    }
}
