//! Persistent session metadata and user-facing lifecycle operations.

use std::{
    env, fs,
    io::{IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use codex_start_core::SessionExitBehavior;
use codex_start_core::UnixArgument;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    cli::{OutputFormat, SessionCommand, SessionRecoveryCommand, SessionSelection},
    configuration::ConfigContext,
    error::{HostError, Result},
    paths::{atomic_write, create_private_dir, ensure_regular_file_or_missing},
    runtime::{Runtime, RuntimeKind},
};

const SESSION_SCHEMA_VERSION: u32 = 1;
const SESSION_FILE: &str = "session.json";
const SESSION_LOG: &str = "session.log";
const SSH_TARGET_FILE: &str = "ssh-agent-target.json";
const APP_SERVER_SOCKET: &str = "/tmp/codex-start-app-server.sock";
const MANAGED_LABEL: &str = "io.codex-start.managed";
const SESSION_LABEL: &str = "io.codex-start.session";

/// Execution behavior associated with one persistent record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Codex app-server plus reconnecting TUI clients.
    Interactive,
    /// Exact arbitrary Codex command, never replayed after reboot.
    Job,
}

/// Durable session lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Detached,
    WaitingForEngine,
    WaitingForSecrets,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

/// Short session mutations used by the interactive manager.
#[derive(Clone, Copy, Debug)]
pub enum SessionMutation {
    Refresh,
    Stop,
    Restart,
    Remove,
}

impl SessionStatus {
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Detached | Self::WaitingForEngine
        )
    }
}

/// Redactable, versioned metadata for one managed session.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: Uuid,
    pub alias: String,
    pub project_id: String,
    pub environment: String,
    pub home: String,
    pub kind: SessionKind,
    pub status: SessionStatus,
    pub runtime: RuntimeKind,
    pub runtime_program: UnixArgument,
    pub container_name: String,
    pub supervisor_pid: Option<u32>,
    pub cwd: UnixArgument,
    pub container_workdir: UnixArgument,
    pub codex_thread_id: Option<Uuid>,
    pub exit_code: Option<u8>,
    pub created_unix_seconds: u64,
    pub updated_unix_seconds: u64,
    /// Most recently validated host agent path. This is omitted from public output.
    #[serde(default)]
    ssh_auth_sock: Option<UnixArgument>,
    #[serde(default)]
    new_client_command: Vec<UnixArgument>,
    #[serde(default)]
    resume_client_command: Vec<UnixArgument>,
    #[serde(default)]
    client_started: bool,
    #[serde(default)]
    on_tui_exit: SessionExitBehavior,
}

impl SessionRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        alias: String,
        project_id: String,
        environment: String,
        home: String,
        kind: SessionKind,
        runtime: RuntimeKind,
        runtime_program: UnixArgument,
        container_name: String,
        cwd: UnixArgument,
        container_workdir: UnixArgument,
    ) -> Self {
        let now = unix_seconds();
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            id: Uuid::new_v4(),
            alias,
            project_id,
            environment,
            home,
            kind,
            status: SessionStatus::Starting,
            runtime,
            runtime_program,
            container_name,
            supervisor_pid: None,
            cwd,
            container_workdir,
            codex_thread_id: None,
            exit_code: None,
            created_unix_seconds: now,
            updated_unix_seconds: now,
            ssh_auth_sock: None,
            new_client_command: Vec::new(),
            resume_client_command: Vec::new(),
            client_started: false,
            on_tui_exit: SessionExitBehavior::Prompt,
        }
    }

    pub fn configure_interactive_client(
        &mut self,
        new_command: Vec<UnixArgument>,
        resume_command: Vec<UnixArgument>,
        on_exit: SessionExitBehavior,
    ) {
        self.new_client_command = new_command;
        self.resume_client_command = resume_command;
        self.on_tui_exit = on_exit;
    }

    pub fn public_value(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": self.schema_version,
            "id": self.id,
            "alias": self.alias,
            "project_id": self.project_id,
            "environment": self.environment,
            "home": self.home,
            "kind": self.kind,
            "status": self.status,
            "runtime": self.runtime,
            "container": self.container_name,
            "supervisor_pid": self.supervisor_pid,
            "cwd": self.cwd,
            "container_workdir": self.container_workdir,
            "codex_thread_id": self.codex_thread_id,
            "exit_code": self.exit_code,
            "created_unix_seconds": self.created_unix_seconds,
            "updated_unix_seconds": self.updated_unix_seconds,
            "ssh_agent_configured": self.ssh_auth_sock.is_some(),
            "interactive_client_started": self.client_started,
        })
    }
}

/// Filesystem-backed persistent session registry.
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        create_private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn for_context(context: &ConfigContext) -> Result<Self> {
        Self::open(context.paths.sessions_dir())
    }

    pub fn create(&self, record: &SessionRecord) -> Result<PathBuf> {
        let directory = self.session_dir(record.id);
        if directory.exists() {
            return Err(HostError::Runtime(format!(
                "session {} already exists",
                record.id
            )));
        }
        create_private_dir(&directory)?;
        self.write(record)?;
        let log = directory.join(SESSION_LOG);
        fs::File::create(&log).map_err(|source| HostError::io(&log, source))?;
        crate::paths::set_private_file(&log)?;
        Ok(directory)
    }

    pub fn write(&self, record: &SessionRecord) -> Result<()> {
        if record.schema_version != SESSION_SCHEMA_VERSION {
            return Err(HostError::Config(format!(
                "unsupported session schema {}",
                record.schema_version
            )));
        }
        let path = self.session_dir(record.id).join(SESSION_FILE);
        let json = serde_json::to_string_pretty(record)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        atomic_write(&path, &format!("{json}\n"))
    }

    pub fn update(
        &self,
        id: Uuid,
        update: impl FnOnce(&mut SessionRecord),
    ) -> Result<SessionRecord> {
        let mut record = self.read(id)?;
        update(&mut record);
        record.updated_unix_seconds = unix_seconds();
        self.write(&record)?;
        Ok(record)
    }

    pub fn read(&self, id: Uuid) -> Result<SessionRecord> {
        let path = self.session_dir(id).join(SESSION_FILE);
        read_record(&path)
    }

    pub fn contains(&self, id: Uuid) -> Result<bool> {
        ensure_regular_file_or_missing(&self.session_dir(id).join(SESSION_FILE))
    }

    pub fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|source| HostError::io(&self.root, source))? {
            let entry = entry.map_err(|source| HostError::io(&self.root, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| HostError::io(entry.path(), source))?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path().join(SESSION_FILE);
            if ensure_regular_file_or_missing(&path)? {
                records.push(read_record(&path)?);
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.updated_unix_seconds));
        Ok(records)
    }

    pub fn select(&self, selector: &str, project_id: &str) -> Result<SessionRecord> {
        if let Ok(id) = Uuid::parse_str(selector) {
            return self.read(id);
        }
        let matches = self
            .list()?
            .into_iter()
            .filter(|record| record.project_id == project_id && record.alias == selector)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [record] => Ok(record.clone()),
            [] => Err(HostError::NotFound(format!("session {selector:?}"))),
            _ => Err(HostError::Usage(format!(
                "session alias {selector:?} is ambiguous; select its UUID"
            ))),
        }
    }

    pub fn log_path(&self, id: Uuid) -> PathBuf {
        self.session_dir(id).join(SESSION_LOG)
    }

    pub fn ssh_target_path(&self, id: Uuid) -> PathBuf {
        self.session_dir(id).join(SSH_TARGET_FILE)
    }

    pub fn set_ssh_target(&self, id: Uuid, socket: Option<&Path>) -> Result<SessionRecord> {
        let path = self.ssh_target_path(id);
        if let Some(socket) = socket {
            let value = serde_json::to_string(&UnixArgument::from(socket.as_os_str()))
                .map_err(|error| HostError::Serialization(error.to_string()))?;
            atomic_write(&path, &format!("{value}\n"))?;
        } else if ensure_regular_file_or_missing(&path)? {
            fs::remove_file(&path).map_err(|source| HostError::io(&path, source))?;
        }
        self.update(id, |record| {
            record.ssh_auth_sock = socket.map(|path| UnixArgument::from(path.as_os_str()));
        })
    }

    pub fn remove(&self, id: Uuid) -> Result<()> {
        let directory = self.session_dir(id);
        let metadata =
            fs::symlink_metadata(&directory).map_err(|source| HostError::io(&directory, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HostError::UnsafePath {
                path: directory,
                reason: "session state must be a real directory".to_owned(),
            });
        }
        fs::remove_dir_all(&directory).map_err(|source| HostError::io(&directory, source))
    }

    fn session_dir(&self, id: Uuid) -> PathBuf {
        self.root.join(id.simple().to_string())
    }
}

/// Execute all session operations except `start`, which routes through the app.
pub fn execute(
    context: &ConfigContext,
    command: SessionCommand,
    output: OutputFormat,
) -> Result<u8> {
    let store = SessionStore::for_context(context)?;
    let project_id = context.repo.as_ref().map_or_else(
        || codex_start_core::canonical_path_hash(context.project_root()),
        |repo| repo.project_id.clone(),
    );
    match command {
        SessionCommand::Start(_) => Err(HostError::Usage(
            "internal error: session start was not routed through run".to_owned(),
        )),
        SessionCommand::List { all } => list(&store, &project_id, all, output),
        SessionCommand::Show(selection) => {
            let record = selected(&store, &selection, &project_id)?;
            emit(output, &record.public_value(), &render_record(&record))?;
            Ok(0)
        }
        SessionCommand::Logs { selection, follow } => {
            let record = selected(&store, &selection, &project_id)?;
            follow_log(&store, record.id, follow)
        }
        SessionCommand::Refresh(selection) => {
            let record = selected(&store, &selection, &project_id)?;
            let refreshed = refresh_ssh(&store, &record)?;
            emit(
                output,
                &refreshed.public_value(),
                &format!("refreshed host integrations for {}", refreshed.id),
            )?;
            Ok(0)
        }
        SessionCommand::Attach {
            selection,
            no_refresh_ssh,
        } => {
            let mut record = selected(&store, &selection, &project_id)?;
            if !no_refresh_ssh {
                record = refresh_ssh(&store, &record)?;
            }
            attach_record(&store, &record)
        }
        SessionCommand::Stop(selection) => {
            let record = selected(&store, &selection, &project_id)?;
            stop(&store, &record, output)
        }
        SessionCommand::Restart(selection) => {
            let record = selected(&store, &selection, &project_id)?;
            restart(&store, &record, output)
        }
        SessionCommand::Remove { selection, force } => {
            let record = selected(&store, &selection, &project_id)?;
            remove(&store, &record, force, output)
        }
        SessionCommand::Recovery { command } => recovery(context, &store, command, output),
    }
}

/// Execute a short lifecycle operation without writing command output.
pub fn mutate(context: &ConfigContext, id: Uuid, mutation: SessionMutation) -> Result<String> {
    let store = SessionStore::for_context(context)?;
    let record = store.read(id)?;
    match mutation {
        SessionMutation::Refresh => {
            refresh_ssh(&store, &record)?;
            Ok(format!("Refreshed host integrations for {}", record.alias))
        }
        SessionMutation::Stop => {
            stop_record(&store, &record)?;
            Ok(format!("Stopped session {}", record.alias))
        }
        SessionMutation::Restart => {
            restart_record(&store, &record)?;
            Ok(format!("Restarted session {}", record.alias))
        }
        SessionMutation::Remove => {
            remove_record(&store, &record, false)?;
            Ok(format!("Removed session {}", record.alias))
        }
    }
}

fn list(store: &SessionStore, project_id: &str, all: bool, output: OutputFormat) -> Result<u8> {
    let records = store
        .list()?
        .into_iter()
        .filter(|record| all || record.project_id == project_id)
        .collect::<Vec<_>>();
    let values = records
        .iter()
        .map(SessionRecord::public_value)
        .collect::<Vec<_>>();
    let human = if records.is_empty() {
        "No codex-start sessions.".to_owned()
    } else {
        records
            .iter()
            .map(render_record)
            .collect::<Vec<_>>()
            .join("\n")
    };
    emit(output, &values, &human)?;
    Ok(0)
}

fn selected(
    store: &SessionStore,
    selection: &SessionSelection,
    project_id: &str,
) -> Result<SessionRecord> {
    store.select(&selection.session, project_id)
}

fn runtime(record: &SessionRecord) -> Result<Runtime> {
    Runtime::detect(record.runtime, Some(record.runtime_program.as_os_str()))
}

fn require_owned(runtime: &Runtime, record: &SessionRecord) -> Result<()> {
    let managed = runtime.container_label(&record.container_name, MANAGED_LABEL)?;
    let session = runtime.container_label(&record.container_name, SESSION_LABEL)?;
    if managed.as_deref() == Some("true") && session.as_deref() == Some(&record.id.to_string()) {
        Ok(())
    } else {
        Err(HostError::Runtime(format!(
            "refusing session operation on container {:?}: ownership labels do not match",
            record.container_name
        )))
    }
}

fn owned_session_containers(runtime: &Runtime, record: &SessionRecord) -> Result<Vec<String>> {
    let rows = runtime.list_containers(&format!("{SESSION_LABEL}={}", record.id), true)?;
    let mut names = rows
        .stdout_text()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .get("Names")
                .or_else(|| value.get("Name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    if runtime.container_state(&record.container_name)?.is_some()
        && !names.contains(&record.container_name)
    {
        names.push(record.container_name.clone());
    }
    let expected_session = record.id.to_string();
    for name in &names {
        let managed = runtime.container_label(name, MANAGED_LABEL)?;
        let session = runtime.container_label(name, SESSION_LABEL)?;
        if managed.as_deref() != Some("true")
            || session.as_deref() != Some(expected_session.as_str())
        {
            return Err(HostError::Runtime(format!(
                "refusing grouped session operation on container {name:?}: ownership labels do not match"
            )));
        }
    }
    Ok(names)
}

fn refresh_ssh(store: &SessionStore, record: &SessionRecord) -> Result<SessionRecord> {
    let socket = current_ssh_socket();
    store.set_ssh_target(record.id, socket.as_deref())
}

pub fn current_ssh_socket() -> Option<PathBuf> {
    env::var_os("SSH_AUTH_SOCK")
        .map(PathBuf::from)
        .filter(|path| is_socket(path))
}

pub fn attach_record(store: &SessionStore, record: &SessionRecord) -> Result<u8> {
    let runtime = runtime(record)?;
    require_owned(&runtime, record)?;
    if runtime.container_state(&record.container_name)? != Some(true) {
        let exit_code = runtime.container_exit_code(&record.container_name)?;
        store.update(record.id, |current| {
            current.exit_code = exit_code;
            current.status = match exit_code {
                Some(0) => SessionStatus::Completed,
                Some(_) => SessionStatus::Failed,
                None => SessionStatus::Interrupted,
            };
        })?;
        return Err(HostError::Runtime(format!(
            "session {} is not running; use `codex-start session restart {}`",
            record.id, record.id
        )));
    }
    if record.kind == SessionKind::Interactive {
        wait_for_app_server(&runtime, record)?;
    }
    store.update(record.id, |current| {
        current.status = SessionStatus::Running;
        if current.kind == SessionKind::Interactive {
            current.client_started = true;
        }
    })?;
    let status = match record.kind {
        SessionKind::Job => runtime.attach(&record.container_name)?,
        SessionKind::Interactive => {
            let command = if record.client_started {
                &record.resume_client_command
            } else {
                &record.new_client_command
            };
            let argv = command
                .iter()
                .map(|argument| argument.as_os_str().to_owned())
                .collect::<Vec<_>>();
            runtime.exec(
                &record.container_name,
                Some(Path::new(record.container_workdir.as_os_str())),
                &argv,
                true,
            )?
        }
    };
    let stop_on_exit = record.kind == SessionKind::Interactive
        && match record.on_tui_exit {
            SessionExitBehavior::Stop => true,
            SessionExitBehavior::Prompt
                if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() =>
            {
                dialoguer::Confirm::new()
                    .with_prompt("Stop the persistent session? (No detaches)")
                    .default(false)
                    .interact()
                    .map_err(|error| HostError::Runtime(format!("session exit prompt: {error}")))?
            }
            SessionExitBehavior::Prompt | SessionExitBehavior::Detach => false,
        };
    if stop_on_exit {
        for container in owned_session_containers(&runtime, record)? {
            if runtime.container_state(&container)? == Some(true) {
                runtime.stop_container(&container)?;
            }
        }
        store.update(record.id, |current| current.status = SessionStatus::Stopped)?;
    } else {
        store.update(record.id, |current| {
            current.status = SessionStatus::Detached;
        })?;
    }
    Ok(status)
}

fn stop(store: &SessionStore, record: &SessionRecord, output: OutputFormat) -> Result<u8> {
    let record = stop_record(store, record)?;
    emit(
        output,
        &record.public_value(),
        &format!("stopped session {}", record.id),
    )?;
    Ok(0)
}

fn stop_record(store: &SessionStore, record: &SessionRecord) -> Result<SessionRecord> {
    let runtime = runtime(record)?;
    for container in owned_session_containers(&runtime, record)? {
        if runtime.container_state(&container)? == Some(true) {
            runtime.stop_container(&container)?;
        }
    }
    store.update(record.id, |current| current.status = SessionStatus::Stopped)
}

fn restart(store: &SessionStore, record: &SessionRecord, output: OutputFormat) -> Result<u8> {
    let record = restart_record(store, record)?;
    emit(
        output,
        &record.public_value(),
        &format!("restarted session {}", record.id),
    )?;
    Ok(0)
}

fn restart_record(store: &SessionStore, record: &SessionRecord) -> Result<SessionRecord> {
    if record.kind != SessionKind::Interactive {
        return Err(HostError::Usage(
            "non-interactive jobs are never replayed automatically".to_owned(),
        ));
    }
    let runtime = runtime(record)?;
    require_owned(&runtime, record)?;
    start_owned_session_containers(&runtime, record)?;
    store.update(record.id, |current| {
        current.status = SessionStatus::Detached;
    })
}

fn remove(
    store: &SessionStore,
    record: &SessionRecord,
    force: bool,
    output: OutputFormat,
) -> Result<u8> {
    remove_record(store, record, force)?;
    emit(
        output,
        &serde_json::json!({"removed": record.id}),
        &format!("removed session {}", record.id),
    )?;
    Ok(0)
}

fn remove_record(store: &SessionStore, record: &SessionRecord, force: bool) -> Result<()> {
    let runtime = runtime(record)?;
    let containers = owned_session_containers(&runtime, record)?;
    if !force
        && containers
            .iter()
            .any(|name| runtime.container_state(name).ok() == Some(Some(true)))
    {
        return Err(HostError::Usage(format!(
            "session {} is running; stop it first or add --force",
            record.id
        )));
    }
    for container in containers {
        runtime.remove_container(&container, force)?;
    }
    let expected_session = record.id.to_string();
    for network in runtime.list_network_names(&format!("{SESSION_LABEL}={expected_session}"))? {
        let managed = runtime.network_label(&network, MANAGED_LABEL)?;
        let session = runtime.network_label(&network, SESSION_LABEL)?;
        if managed.as_deref() != Some("true")
            || session.as_deref() != Some(expected_session.as_str())
        {
            return Err(HostError::Runtime(format!(
                "refusing grouped session operation on network {network:?}: ownership labels do not match"
            )));
        }
        runtime.remove_network(&network)?;
    }
    for volume in runtime.list_volume_names(&format!("{SESSION_LABEL}={expected_session}"))? {
        let managed = runtime.volume_label(&volume, MANAGED_LABEL)?;
        let session = runtime.volume_label(&volume, SESSION_LABEL)?;
        if managed.as_deref() != Some("true")
            || session.as_deref() != Some(expected_session.as_str())
        {
            return Err(HostError::Runtime(format!(
                "refusing grouped session operation on volume {volume:?}: ownership labels do not match"
            )));
        }
        runtime.remove_volume(&volume, true)?;
    }
    store.remove(record.id)?;
    Ok(())
}

fn recovery(
    context: &ConfigContext,
    store: &SessionStore,
    command: SessionRecoveryCommand,
    output: OutputFormat,
) -> Result<u8> {
    let service = recovery_service_path(context)?;
    match command {
        SessionRecoveryCommand::Enable => {
            install_recovery_service(&service)?;
            emit(
                output,
                &serde_json::json!({"enabled": true, "service": service}),
                "installed and enabled session recovery",
            )?;
        }
        SessionRecoveryCommand::Disable => {
            uninstall_recovery_service(&service)?;
            emit(
                output,
                &serde_json::json!({"enabled": false}),
                "disabled and removed session recovery",
            )?;
        }
        SessionRecoveryCommand::Status => {
            let enabled = ensure_regular_file_or_missing(&service)?;
            emit(
                output,
                &serde_json::json!({"enabled": enabled, "service": service}),
                if enabled {
                    "session recovery is enabled"
                } else {
                    "session recovery is disabled"
                },
            )?;
        }
        SessionRecoveryCommand::Run => return run_recovery_loop(store),
    }
    Ok(0)
}

fn recovery_service_path(context: &ConfigContext) -> Result<PathBuf> {
    let _ = context;
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| HostError::Config("HOME is not set".to_owned()))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join("io.codex-start.session-recovery.plist"))
    }
    #[cfg(target_os = "linux")]
    {
        let config_home =
            context.paths.config.parent().ok_or_else(|| {
                HostError::Config("codex-start config root has no parent".to_owned())
            })?;
        Ok(config_home
            .join("systemd/user")
            .join("codex-start-session-recovery.service"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = context;
        Err(HostError::Config(
            "session recovery is supported only on macOS and Linux".to_owned(),
        ))
    }
}

#[cfg(not(windows))]
pub fn recovery_installation(context: &ConfigContext) -> Result<(bool, PathBuf)> {
    let path = recovery_service_path(context)?;
    Ok((ensure_regular_file_or_missing(&path)?, path))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn install_recovery_service(path: &Path) -> Result<()> {
    let executable =
        env::current_exe().map_err(|source| HostError::io("current executable", source))?;
    let executable = executable.to_str().ok_or_else(|| {
        HostError::Config("recovery service executable path must be valid UTF-8".to_owned())
    })?;
    #[cfg(target_os = "macos")]
    {
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>io.codex-start.session-recovery</string>\n<key>ProgramArguments</key><array><string>{}</string><string>session</string><string>recovery</string><string>run</string></array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n</dict></plist>\n",
            xml_escape(executable)
        );
        atomic_write(path, &contents)?;
        let domain = format!("gui/{}", current_uid()?);
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain, path.to_string_lossy().as_ref()])
            .status();
        command_success(
            std::process::Command::new("launchctl").args([
                "bootstrap",
                &domain,
                path.to_string_lossy().as_ref(),
            ]),
            "launchctl bootstrap",
        )?;
    }
    #[cfg(target_os = "linux")]
    {
        let contents = format!(
            "[Unit]\nDescription=Recover codex-start sessions when the container engine becomes available\n\n[Service]\nExecStart={} session recovery run\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            systemd_quote(executable)?
        );
        atomic_write(path, &contents)?;
        command_success(
            std::process::Command::new("systemctl").args(["--user", "daemon-reload"]),
            "systemctl --user daemon-reload",
        )?;
        command_success(
            std::process::Command::new("systemctl").args([
                "--user",
                "enable",
                "--now",
                "codex-start-session-recovery.service",
            ]),
            "systemctl --user enable",
        )?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_recovery_service(_path: &Path) -> Result<()> {
    Err(HostError::Config(
        "session recovery is supported only on macOS and Linux".to_owned(),
    ))
}

fn uninstall_recovery_service(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if ensure_regular_file_or_missing(path)? {
        let domain = format!("gui/{}", current_uid()?);
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain, path.to_string_lossy().as_ref()])
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "disable",
                "--now",
                "codex-start-session-recovery.service",
            ])
            .status();
    }
    if ensure_regular_file_or_missing(path)? {
        fs::remove_file(path).map_err(|source| HostError::io(path, source))?;
    }
    #[cfg(target_os = "linux")]
    command_success(
        std::process::Command::new("systemctl").args(["--user", "daemon-reload"]),
        "systemctl --user daemon-reload",
    )?;
    Ok(())
}

fn run_recovery_loop(store: &SessionStore) -> Result<u8> {
    loop {
        for record in store.list()? {
            reconcile_session(store, &record)?;
        }
        thread::sleep(Duration::from_secs(5));
    }
}

fn reconcile_session(store: &SessionStore, record: &SessionRecord) -> Result<()> {
    if !record.status.is_live() && record.status != SessionStatus::Interrupted {
        return Ok(());
    }
    let runtime = match runtime(record) {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(session = %record.id, %error, "waiting for session runtime");
            if record.status != SessionStatus::WaitingForEngine {
                store.update(record.id, |current| {
                    current.status = SessionStatus::WaitingForEngine;
                })?;
            }
            return Ok(());
        }
    };
    match runtime.container_state(&record.container_name)? {
        Some(true) => {
            if matches!(
                record.status,
                SessionStatus::Starting
                    | SessionStatus::WaitingForEngine
                    | SessionStatus::Interrupted
            ) {
                store.update(record.id, |current| {
                    current.status = SessionStatus::Detached;
                })?;
            }
        }
        Some(false) if record.kind == SessionKind::Interactive => {
            require_owned(&runtime, record)?;
            start_owned_session_containers(&runtime, record)?;
            store.update(record.id, |current| {
                current.status = SessionStatus::Detached;
            })?;
        }
        Some(false) => {
            let code = runtime.container_exit_code(&record.container_name)?;
            store.update(record.id, |current| {
                current.exit_code = code;
                current.status = code.map_or(SessionStatus::Interrupted, |code| {
                    if code == 0 {
                        SessionStatus::Completed
                    } else {
                        SessionStatus::Failed
                    }
                });
            })?;
        }
        None => {
            if record.status != SessionStatus::Interrupted {
                store.update(record.id, |current| {
                    current.status = SessionStatus::Interrupted;
                })?;
            }
        }
    }
    Ok(())
}

fn start_owned_session_containers(runtime: &Runtime, record: &SessionRecord) -> Result<()> {
    let mut containers = owned_session_containers(runtime, record)?;
    containers.sort_by_key(|name| name == &record.container_name);
    for container in containers {
        if runtime.container_state(&container)? == Some(false) {
            runtime.start_container(&container)?;
        }
    }
    Ok(())
}

fn wait_for_app_server(runtime: &Runtime, record: &SessionRecord) -> Result<()> {
    let probe = [
        std::ffi::OsString::from("test"),
        std::ffi::OsString::from("-S"),
        std::ffi::OsString::from(APP_SERVER_SOCKET),
    ];
    for _ in 0..100 {
        if runtime.container_state(&record.container_name)? != Some(true) {
            break;
        }
        if runtime.exec_probe(&record.container_name, &probe)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(HostError::Runtime(format!(
        "session {} app-server did not become ready within 10 seconds",
        record.id
    )))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command_success(command: &mut std::process::Command, label: &str) -> Result<()> {
    let status = command.status().map_err(|source| HostError::CommandIo {
        program: label.into(),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(HostError::Runtime(format!("{label} failed with {status}")))
    }
}

#[cfg(target_os = "macos")]
fn current_uid() -> Result<String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|source| HostError::CommandIo {
            program: "id".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(HostError::Runtime("id -u failed".to_owned()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> Result<String> {
    if value.contains(['\0', '\n', '\r']) {
        return Err(HostError::Config(
            "recovery executable path contains unsupported control characters".to_owned(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn follow_log(store: &SessionStore, id: Uuid, follow: bool) -> Result<u8> {
    let path = store.log_path(id);
    let mut file = fs::File::open(&path).map_err(|source| HostError::io(&path, source))?;
    let mut position = 0;
    loop {
        file.seek(SeekFrom::Start(position))
            .map_err(|source| HostError::io(&path, source))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| HostError::io(&path, source))?;
        if !bytes.is_empty() {
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|source| HostError::io("stdout", source))?;
            std::io::stdout()
                .flush()
                .map_err(|source| HostError::io("stdout", source))?;
            position += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        }
        if !follow || !store.read(id)?.status.is_live() {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(0)
}

fn read_record(path: &Path) -> Result<SessionRecord> {
    const MAX_RECORD_BYTES: u64 = 1_048_576;
    let metadata = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES
    {
        return Err(HostError::UnsafePath {
            path: path.to_path_buf(),
            reason: "session record must be a bounded regular file".to_owned(),
        });
    }
    let bytes = fs::read(path).map_err(|source| HostError::io(path, source))?;
    let record: SessionRecord = serde_json::from_slice(&bytes)
        .map_err(|error| HostError::Serialization(format!("{}: {error}", path.display())))?;
    if record.schema_version != SESSION_SCHEMA_VERSION {
        return Err(HostError::Config(format!(
            "{} uses unsupported session schema {}",
            path.display(),
            record.schema_version
        )));
    }
    Ok(record)
}

fn render_record(record: &SessionRecord) -> String {
    format!(
        "{}\t{}\t{:?}\t{:?}\t{}",
        record.id, record.alias, record.kind, record.status, record.environment
    )
}

fn emit<T: Serialize>(output: OutputFormat, value: &T, human: &str) -> Result<()> {
    match output {
        OutputFormat::Human => println!("{human}"),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(value)
                .map_err(|error| HostError::Serialization(error.to_string()))?
        ),
    }
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
    })
}

#[cfg(not(unix))]
fn is_socket(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{SessionKind, SessionRecord, SessionStatus, SessionStore};
    use crate::runtime::RuntimeKind;
    use codex_start_core::UnixArgument;

    fn record(alias: &str) -> SessionRecord {
        SessionRecord::new(
            alias.to_owned(),
            "project-id".to_owned(),
            "rust".to_owned(),
            "default".to_owned(),
            SessionKind::Interactive,
            RuntimeKind::Docker,
            UnixArgument::from("docker"),
            format!("container-{alias}"),
            UnixArgument::from("/workspace"),
            UnixArgument::from("/workspace"),
        )
    }

    #[test]
    fn records_round_trip_and_aliases_are_project_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("sessions")).unwrap();
        let first = record("feature");
        store.create(&first).unwrap();
        assert_eq!(store.read(first.id).unwrap().alias, "feature");
        assert_eq!(store.select("feature", "project-id").unwrap().id, first.id);
        assert!(store.select("feature", "another-project").is_err());
    }

    #[test]
    fn updates_are_atomic_and_public_output_redacts_agent_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path().join("sessions")).unwrap();
        let first = record("feature");
        store.create(&first).unwrap();
        let updated = store
            .update(first.id, |record| record.status = SessionStatus::Detached)
            .unwrap();
        assert_eq!(updated.status, SessionStatus::Detached);
        let public = updated.public_value().to_string();
        assert!(!public.contains("SSH_AUTH_SOCK"));
    }
}
