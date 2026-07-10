//! Public command-line interface.

use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    str::FromStr,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::{app, error::Result, runtime::RuntimeKind};

/// Run Codex in reproducible containerized environments.
#[derive(Clone, Debug, Parser)]
#[command(name = "codex-start", version, propagate_version = true)]
#[command(about = "Run Codex in reproducible Docker or Podman development environments")]
pub struct Cli {
    /// Increase diagnostic verbosity (`-vv` enables trace logs).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress informational output other than errors.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Select human-readable or machine-readable output.
    #[arg(long, value_enum, default_value_t, global = true)]
    pub output: OutputFormat,

    /// Use an alternate global configuration file.
    #[arg(long, global = true, env = "CODEX_START_CONFIG")]
    pub config: Option<PathBuf>,

    /// Legacy pi-start-compatible flags.
    #[command(flatten)]
    pub legacy: LegacyOptions,

    /// Operation to perform. An unknown command is treated as a legacy environment name.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level operations.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Start Codex in a selected development environment.
    Run(RunArgs),
    /// Merge branches or managed worktrees into the current branch using a Codex agent.
    Merge(MergeArgs),
    /// Start or attach to a login shell.
    Shell(ShellArgs),
    /// Manage worktree changes and lifecycle.
    Worktree(WorktreeArgs),
    /// Inspect and clean owned runtime resources.
    Resources(ResourcesArgs),
    /// Inspect, build, and update environments.
    Env(EnvironmentArgs),
    /// Manage shared Codex homes.
    Home(HomeArgs),
    /// Manage persistent background and resumable sessions.
    Session(SessionArgs),
    /// Open the interactive editor or inspect and edit configuration directly.
    Config(ConfigArgs),
    /// Diagnose host, runtime, image, and Codex compatibility.
    Doctor(DoctorArgs),
    /// Legacy invocation: the first value is an environment and the remainder are Codex args.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Output serialization format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Concise terminal-oriented output.
    #[default]
    Human,
    /// Stable newline-delimited JSON objects.
    Json,
}

/// Common run overrides.
#[derive(Clone, Debug, Default, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct RunOptions {
    /// Name the linked worktree and owned container.
    #[arg(short = 'n', long)]
    pub name: Option<String>,

    /// Select Docker, Podman, or automatic detection.
    #[arg(long, value_enum)]
    pub runtime: Option<RuntimeKind>,

    /// Override the runtime executable path.
    #[arg(long)]
    pub runtime_program: Option<PathBuf>,

    /// Select a named managed, host, or path Codex home.
    #[arg(long)]
    pub home: Option<String>,

    /// Select an egress mode.
    #[arg(long, value_enum)]
    pub network: Option<NetworkModeArg>,

    /// Disable all egress.
    #[arg(long, conflicts_with = "network")]
    pub offline: bool,

    /// Deprecated alias selecting allowlist networking.
    #[arg(long, conflicts_with_all = ["network", "offline"])]
    pub no_network: bool,

    /// Mount the current worktree directly.
    #[arg(long, conflicts_with = "worktree")]
    pub no_worktree: bool,

    /// Require linked-worktree mode.
    #[arg(long, conflicts_with = "no_worktree")]
    pub worktree: bool,

    /// Publish a container port to the host. Repeatable.
    #[arg(short = 'p', long = "publish", value_name = "SPEC")]
    pub publish: Vec<PortSpec>,

    /// Rebuild the environment image even when the content hash exists.
    #[arg(long)]
    pub rebuild: bool,

    /// Pull locked images instead of building when possible.
    #[arg(long)]
    pub pull: bool,

    /// Do not allocate a pseudo-terminal.
    #[arg(long)]
    pub no_tty: bool,

    /// Print the redacted execution plan without changing runtime state.
    #[arg(long)]
    pub dry_run: bool,

    /// Force persistent session management for this run.
    #[arg(long, conflicts_with = "ephemeral")]
    pub persistent: bool,

    /// Use the foreground disposable lifecycle for this run.
    #[arg(long, conflicts_with = "persistent")]
    pub ephemeral: bool,

    /// Additional allowed egress host or host:port pattern.
    #[arg(long = "allow-host")]
    pub allow_hosts: Vec<String>,

    /// Expert engine argument; reduces portability. Repeatable.
    #[arg(long = "runtime-arg", allow_hyphen_values = true)]
    pub runtime_args: Vec<OsString>,
}

/// Launch arguments.
#[derive(Clone, Debug, Args)]
pub struct RunArgs {
    /// Environment name. Auto-detect when omitted.
    #[arg(value_name = "ENVIRONMENT")]
    pub environment: Option<String>,

    /// Runner-specific options.
    #[command(flatten)]
    pub options: RunOptions,

    /// Arguments passed verbatim to Codex.
    #[arg(last = true, allow_hyphen_values = true)]
    pub codex_args: Vec<OsString>,
}

/// Conflict-resolution merge-agent arguments.
#[derive(Clone, Debug, Args)]
pub struct MergeArgs {
    /// Environment name. Uses configured detection when omitted.
    #[arg(long)]
    pub environment: Option<String>,

    /// Model used only for this merge-agent task.
    #[arg(long)]
    pub model: Option<String>,

    /// Container runner overrides.
    #[command(flatten)]
    pub options: MergeRunOptions,

    /// Ordered local branch names or managed worktree names to merge.
    #[arg(value_name = "SOURCE", required = true)]
    pub sources: Vec<String>,
}

/// Runner overrides available to the fixed-current-worktree merge mode.
#[derive(Clone, Debug, Default, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct MergeRunOptions {
    /// Select Docker, Podman, or automatic detection.
    #[arg(long, value_enum)]
    pub runtime: Option<RuntimeKind>,

    /// Override the runtime executable path.
    #[arg(long)]
    pub runtime_program: Option<PathBuf>,

    /// Select a named managed, host, or path Codex home.
    #[arg(long)]
    pub home: Option<String>,

    /// Select an egress mode.
    #[arg(long, value_enum)]
    pub network: Option<NetworkModeArg>,

    /// Disable all egress.
    #[arg(long, conflicts_with = "network")]
    pub offline: bool,

    /// Deprecated alias selecting allowlist networking.
    #[arg(long, conflicts_with_all = ["network", "offline"])]
    pub no_network: bool,

    /// Publish a container port to the host. Repeatable.
    #[arg(short = 'p', long = "publish", value_name = "SPEC")]
    pub publish: Vec<PortSpec>,

    /// Rebuild the environment image even when the content hash exists.
    #[arg(long)]
    pub rebuild: bool,

    /// Pull locked images instead of building when possible.
    #[arg(long)]
    pub pull: bool,

    /// Do not allocate a pseudo-terminal.
    #[arg(long)]
    pub no_tty: bool,

    /// Print the redacted execution plan without changing runtime or Git state.
    #[arg(long)]
    pub dry_run: bool,

    /// Additional allowed egress host or host:port pattern.
    #[arg(long = "allow-host")]
    pub allow_hosts: Vec<String>,

    /// Expert engine argument; reduces portability. Repeatable.
    #[arg(long = "runtime-arg", allow_hyphen_values = true)]
    pub runtime_args: Vec<OsString>,
}

/// Shell arguments.
#[derive(Clone, Debug, Args)]
#[allow(clippy::struct_field_names)]
pub struct ShellArgs {
    /// Environment name. Auto-detect when omitted.
    pub environment: Option<String>,

    /// Runner-specific options.
    #[command(flatten)]
    pub options: RunOptions,

    /// Shell executable and arguments, defaulting to `bash -l`.
    #[arg(last = true, allow_hyphen_values = true)]
    pub shell_args: Vec<OsString>,
}

/// Worktree command group.
#[derive(Clone, Debug, Args)]
pub struct WorktreeArgs {
    /// Worktree operation.
    #[command(subcommand)]
    pub command: WorktreeCommand,
}

/// Worktree lifecycle operations.
#[derive(Clone, Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Run interactive Git commit in a selected worktree.
    Commit(WorktreeSelection),
    /// Autosave and squash a selected worktree into the current branch.
    Squash(WorktreeSelection),
    /// Apply selected worktree changes without committing.
    Move(WorktreeSelection),
    /// Open a selected worktree in the configured editor.
    Edit(WorktreeSelection),
    /// Remove all owned worktrees and branches for the current project.
    Cleanup {
        /// Also remove dirty worktrees and unmerged owned branches.
        #[arg(long)]
        force: bool,
    },
}

/// Select a named worktree or the most recently modified one.
#[derive(Clone, Debug, Args)]
pub struct WorktreeSelection {
    /// Worktree name.
    #[arg(short, long)]
    pub name: Option<String>,
}

/// Runtime resource command group.
#[derive(Clone, Debug, Args)]
pub struct ResourcesArgs {
    /// Runtime override.
    #[arg(long, value_enum)]
    pub runtime: Option<RuntimeKind>,

    /// Runtime resource operation.
    #[command(subcommand)]
    pub command: ResourcesCommand,
}

/// Owned runtime resource operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ResourcesCommand {
    /// List owned workload and sidecar containers.
    List,
    /// Stream or print logs from an owned container.
    Logs {
        /// Container name.
        name: String,
        /// Continue following log output.
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop an owned workload and its sidecars.
    Stop {
        /// Container name.
        name: String,
    },
    /// Remove stopped/stale owned containers and networks.
    Cleanup {
        /// Also stop and remove running workloads.
        #[arg(long)]
        force: bool,
    },
}

/// Environment command group.
#[derive(Clone, Debug, Args)]
pub struct EnvironmentArgs {
    /// Environment operation.
    #[command(subcommand)]
    pub command: EnvironmentCommand,
}

/// Environment operations.
#[derive(Clone, Debug, Subcommand)]
pub enum EnvironmentCommand {
    /// List built-in and user environments.
    List,
    /// Print a resolved environment and provenance.
    Show { name: String },
    /// Build one environment image.
    Build {
        name: String,
        #[arg(long, value_enum)]
        runtime: Option<RuntimeKind>,
        #[arg(long)]
        no_cache: bool,
    },
    /// Refresh the user image/version lock after validation.
    Update {
        /// Check and report available updates without writing.
        #[arg(long)]
        check: bool,
    },
}

/// Codex home command group.
#[derive(Clone, Debug, Args)]
pub struct HomeArgs {
    /// Home operation.
    #[command(subcommand)]
    pub command: HomeCommand,
}

/// Codex home operations.
#[derive(Clone, Debug, Subcommand)]
pub enum HomeCommand {
    /// List configured and discovered homes.
    List,
    /// Create a managed home.
    Create { name: String },
    /// Import a Codex directory into a managed home.
    Import {
        name: String,
        #[arg(long)]
        from: PathBuf,
        /// Companion `.agents` source; inferred beside a `.codex` source when present.
        #[arg(long)]
        agents_from: Option<PathBuf>,
    },
    /// Export a quiescent Codex directory.
    Export {
        name: String,
        #[arg(long)]
        to: PathBuf,
        /// Companion `.agents` destination; inferred beside a `.codex` destination.
        #[arg(long)]
        agents_to: Option<PathBuf>,
    },
    /// Run any Codex CLI command using a home and generic environment.
    Exec {
        name: String,
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        codex_args: Vec<OsString>,
    },
}

/// Persistent-session command group.
#[derive(Clone, Debug, Args)]
pub struct SessionArgs {
    /// Session operation.
    #[command(subcommand)]
    pub command: SessionCommand,
}

/// Persistent-session lifecycle operations.
#[derive(Clone, Debug, Subcommand)]
pub enum SessionCommand {
    /// Start a managed session using the normal run options.
    Start(RunArgs),
    /// List sessions for the current project.
    List {
        /// Include sessions belonging to other projects.
        #[arg(long)]
        all: bool,
    },
    /// Show one session's redacted metadata.
    Show(SessionSelection),
    /// Attach to a live session or its persisted Codex thread.
    Attach {
        #[command(flatten)]
        selection: SessionSelection,
        /// Keep the session's current SSH-agent target.
        #[arg(long)]
        no_refresh_ssh: bool,
    },
    /// Print or follow one session's logs.
    Logs {
        #[command(flatten)]
        selection: SessionSelection,
        /// Continue following new output.
        #[arg(short, long)]
        follow: bool,
    },
    /// Refresh host integrations for a running session.
    Refresh(SessionSelection),
    /// Stop a session while preserving its metadata and worktree.
    Stop(SessionSelection),
    /// Restart a stopped interactive session.
    Restart(SessionSelection),
    /// Remove a stopped session's owned runtime state.
    Remove {
        #[command(flatten)]
        selection: SessionSelection,
        /// Stop a running session before removal.
        #[arg(long)]
        force: bool,
    },
    /// Configure cross-reboot recovery.
    Recovery {
        #[command(subcommand)]
        command: SessionRecoveryCommand,
    },
}

/// Select one persistent session by UUID or project-local alias.
#[derive(Clone, Debug, Args)]
pub struct SessionSelection {
    /// Session UUID or alias.
    pub session: String,
}

/// User-level reboot-recovery service operations.
#[derive(Clone, Copy, Debug, Subcommand)]
pub enum SessionRecoveryCommand {
    /// Install and enable user-level recovery.
    Enable,
    /// Disable and remove user-level recovery.
    Disable,
    /// Report whether recovery is installed and active.
    Status,
    /// Run the long-lived recovery reconciler.
    #[command(hide = true)]
    Run,
}

/// Configuration command group.
#[derive(Clone, Debug, Args)]
pub struct ConfigArgs {
    /// Configuration operation; omit to open the interactive editor.
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

/// Configuration operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create explicit global or project defaults.
    Init {
        /// Initialize global rather than current-project settings.
        #[arg(long)]
        global: bool,
        /// Initial environment; auto-detect when omitted.
        #[arg(long)]
        environment: Option<String>,
        /// Replace an existing file after making a backup.
        #[arg(long)]
        force: bool,
    },
    /// Print the merged configuration with secrets redacted.
    Show,
    /// Print each resolved value and its winning source.
    Explain,
    /// Open global, project, or Codex-home config in an editor.
    Edit {
        #[arg(long)]
        global: bool,
        #[arg(long)]
        codex_home: Option<String>,
    },
    /// Set a typed TOML value using a dotted path.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        global: bool,
    },
}

/// Doctor options.
#[derive(Clone, Copy, Debug, Args)]
pub struct DoctorArgs {
    /// Runtime to diagnose; auto-detect when omitted.
    #[arg(long, value_enum)]
    pub runtime: Option<RuntimeKind>,
    /// Also launch a disposable image and run deeper checks.
    #[arg(long)]
    pub deep: bool,
}

/// Compatibility-only action flags.
#[derive(Clone, Debug, Default, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct LegacyOptions {
    #[arg(short = 'n', long, hide = true)]
    pub name: Option<String>,
    #[arg(long, hide = true)]
    pub no_worktree: bool,
    #[arg(long, hide = true)]
    pub worktree: bool,
    #[arg(long, hide = true)]
    pub no_network: bool,
    #[arg(short = 'p', long = "publish", hide = true)]
    pub publish: Vec<PortSpec>,
    #[arg(long, hide = true, conflicts_with_all = ["squash", "move_changes", "edit", "shell", "cleanup", "cleanup_git"])]
    pub commit: bool,
    #[arg(long, hide = true, conflicts_with_all = ["commit", "move_changes", "edit", "shell", "cleanup", "cleanup_git"])]
    pub squash: bool,
    #[arg(long = "move", hide = true, conflicts_with_all = ["commit", "squash", "edit", "shell", "cleanup", "cleanup_git"])]
    pub move_changes: bool,
    #[arg(long, hide = true, conflicts_with_all = ["commit", "squash", "move_changes", "shell", "cleanup", "cleanup_git"])]
    pub edit: bool,
    #[arg(long, hide = true, conflicts_with_all = ["commit", "squash", "move_changes", "edit", "cleanup", "cleanup_git"])]
    pub shell: bool,
    #[arg(long, hide = true, conflicts_with_all = ["commit", "squash", "move_changes", "edit", "shell", "cleanup_git"])]
    pub cleanup: bool,
    #[arg(long, hide = true, conflicts_with_all = ["commit", "squash", "move_changes", "edit", "shell", "cleanup"])]
    pub cleanup_git: bool,
}

/// Network isolation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NetworkModeArg {
    /// No egress path.
    Offline,
    /// Rust HTTP/CONNECT egress allowlist.
    Allowlist,
    /// Unrestricted runtime bridge networking.
    Bridge,
    /// Runtime host networking.
    Host,
}

/// Validated port publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PortSpec {
    /// Host bind address.
    pub host_ip: IpAddr,
    /// Host port.
    pub host_port: u16,
    /// Container port.
    pub container_port: u16,
    /// Transport protocol.
    pub protocol: PortProtocol,
}

/// Published transport protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

impl FromStr for PortSpec {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.is_empty() || value.chars().any(char::is_whitespace) || value.starts_with('-') {
            return Err("port spec must be non-empty and contain no whitespace".to_owned());
        }
        let (base, protocol) = match value.rsplit_once('/') {
            Some((base, "tcp")) => (base, PortProtocol::Tcp),
            Some((base, "udp")) => (base, PortProtocol::Udp),
            Some((_, protocol)) => {
                return Err(format!(
                    "unsupported port protocol {protocol:?}; use tcp or udp"
                ));
            }
            None => (value, PortProtocol::Tcp),
        };
        parse_port_base(base, protocol)
    }
}

fn parse_port_base(base: &str, protocol: PortProtocol) -> std::result::Result<PortSpec, String> {
    let default_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    if let Some(rest) = base.strip_prefix('[') {
        let (ip, ports) = rest
            .split_once("]:")
            .ok_or_else(|| "IPv6 host addresses must use [address]:host:container".to_owned())?;
        let host_ip = ip
            .parse::<IpAddr>()
            .map_err(|error| format!("invalid host IP: {error}"))?;
        let (host, container) = ports
            .split_once(':')
            .ok_or_else(|| "IPv6 publication requires host and container ports".to_owned())?;
        return build_port(host_ip, host, container, protocol);
    }
    let parts = base.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [port] => build_port(default_ip, port, port, protocol),
        [host, container] => build_port(default_ip, host, container, protocol),
        [ip, host, container] => {
            let host_ip = ip
                .parse::<IpAddr>()
                .map_err(|error| format!("invalid host IP: {error}"))?;
            build_port(host_ip, host, container, protocol)
        }
        _ => Err("expected PORT, HOST:CONTAINER, or IP:HOST:CONTAINER".to_owned()),
    }
}

fn build_port(
    host_ip: IpAddr,
    host: &str,
    container: &str,
    protocol: PortProtocol,
) -> std::result::Result<PortSpec, String> {
    let host_port = parse_port(host, "host")?;
    let container_port = parse_port(container, "container")?;
    Ok(PortSpec {
        host_ip,
        host_port,
        container_port,
        protocol,
    })
}

fn parse_port(value: &str, label: &str) -> std::result::Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| format!("invalid {label} port {value:?}"))?;
    if port == 0 {
        Err(format!("{label} port must be between 1 and 65535"))
    } else {
        Ok(port)
    }
}

/// Dispatch an already parsed command.
pub async fn run(cli: Cli) -> Result<u8> {
    Box::pin(app::run(cli)).await
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        str::FromStr,
    };

    use clap::Parser;

    use super::{Cli, Command, HomeCommand, PortProtocol, PortSpec, SessionCommand};

    #[test]
    fn parses_explicit_passthrough_commands_without_clap_assertions() {
        let run = Cli::try_parse_from(["codex-start", "run", "rust", "--", "exec", "--json"])
            .expect("run parse");
        assert!(matches!(
            run.command,
            Some(Command::Run(args)) if args.codex_args == ["exec", "--json"]
        ));

        let shell = Cli::try_parse_from(["codex-start", "shell", "generic", "--", "zsh", "-l"])
            .expect("shell parse");
        assert!(matches!(
            shell.command,
            Some(Command::Shell(args)) if args.shell_args == ["zsh", "-l"]
        ));

        let home = Cli::try_parse_from([
            "codex-start",
            "home",
            "exec",
            "team",
            "--",
            "login",
            "--device-auth",
        ])
        .expect("home exec parse");
        assert!(matches!(
            home.command,
            Some(Command::Home(args))
                if matches!(&args.command, HomeCommand::Exec { codex_args, .. }
                    if codex_args.as_slice()
                        == [OsString::from("login"), OsString::from("--device-auth")])
        ));
    }

    #[test]
    fn parses_persistent_session_lifecycle_and_ephemeral_override() {
        let run = Cli::try_parse_from(["codex-start", "run", "rust", "--ephemeral"])
            .expect("ephemeral run");
        assert!(matches!(
            run.command,
            Some(Command::Run(args)) if args.options.ephemeral
        ));
        assert!(
            Cli::try_parse_from(["codex-start", "run", "rust", "--ephemeral", "--persistent"])
                .is_err()
        );

        let attach = Cli::try_parse_from([
            "codex-start",
            "session",
            "attach",
            "feature",
            "--no-refresh-ssh",
        ])
        .expect("session attach");
        assert!(matches!(
            attach.command,
            Some(Command::Session(args))
                if matches!(args.command, SessionCommand::Attach { no_refresh_ssh: true, .. })
        ));

        let start = Cli::try_parse_from([
            "codex-start",
            "session",
            "start",
            "rust",
            "--",
            "exec",
            "task",
        ])
        .expect("session start");
        assert!(matches!(
            start.command,
            Some(Command::Session(args))
                if matches!(&args.command, SessionCommand::Start(run) if run.codex_args == ["exec", "task"])
        ));
    }

    #[test]
    fn parses_ordered_merge_sources_and_task_model() {
        let cli = Cli::try_parse_from([
            "codex-start",
            "merge",
            "--environment",
            "rust",
            "--model",
            "merge-model",
            "feature-one",
            "worktree-two",
        ])
        .expect("merge parse");
        assert!(matches!(
            cli.command,
            Some(Command::Merge(args))
                if args.environment.as_deref() == Some("rust")
                    && args.model.as_deref() == Some("merge-model")
                    && args.sources == ["feature-one", "worktree-two"]
        ));
        assert!(Cli::try_parse_from(["codex-start", "merge"]).is_err());
        assert!(Cli::try_parse_from(["codex-start", "merge", "--worktree", "feature"]).is_err());
    }

    #[test]
    fn bare_config_opens_interactive_editor_and_subcommands_still_parse() {
        let interactive =
            Cli::try_parse_from(["codex-start", "config"]).expect("interactive config parse");
        assert!(matches!(
            interactive.command,
            Some(Command::Config(args)) if args.command.is_none()
        ));

        let set = Cli::try_parse_from(["codex-start", "config", "set", "network", "\"offline\""])
            .expect("config set parse");
        assert!(matches!(
            set.command,
            Some(Command::Config(args)) if args.command.is_some()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_passthrough_preserves_non_utf8_argument_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let exact = OsString::from_vec(vec![b'a', 0xFF, b'z']);
        let cli = Cli::try_parse_from([
            OsString::from("codex-start"),
            OsString::from("run"),
            OsString::from("generic"),
            OsString::from("--"),
            OsString::from("exec"),
            exact.clone(),
        ])
        .expect("non-UTF-8 passthrough parse");
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.codex_args[0], "exec");
        assert_eq!(args.codex_args[1].as_os_str().as_bytes(), exact.as_bytes());
    }

    #[test]
    fn parses_legacy_environment_as_external_subcommand() {
        let cli =
            Cli::try_parse_from(["codex-start", "rust", "--model", "test"]).expect("legacy parse");
        assert!(matches!(cli.command, Some(Command::External(values)) if values.len() == 3));
    }

    #[test]
    fn accepts_no_network_after_explicit_run_or_shell() {
        let run = Cli::try_parse_from(["codex-start", "run", "--no-network", "generic"])
            .expect("explicit run alias");
        assert!(matches!(
            run.command,
            Some(Command::Run(args)) if args.options.no_network
        ));
        assert!(
            Cli::try_parse_from([
                "codex-start",
                "shell",
                "--no-network",
                "--offline",
                "generic"
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_shorthand_and_ipv6_ports() {
        assert_eq!(
            PortSpec::from_str("5173").expect("short"),
            PortSpec {
                host_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                host_port: 5173,
                container_port: 5173,
                protocol: PortProtocol::Tcp,
            }
        );
        assert_eq!(
            PortSpec::from_str("[::1]:9000:80/udp").expect("ipv6"),
            PortSpec {
                host_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
                host_port: 9000,
                container_port: 80,
                protocol: PortProtocol::Udp,
            }
        );
    }

    #[test]
    fn rejects_invalid_ports_and_protocols() {
        assert!(PortSpec::from_str("0").is_err());
        assert!(PortSpec::from_str("80/icmp").is_err());
    }
}
