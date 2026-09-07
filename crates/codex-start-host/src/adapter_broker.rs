//! Route one app-server connection to project-specific container processes.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};

use crate::{
    adapter::{ATTACHMENT_RUNTIME_INFO_ENV, launcher_arguments},
    command::{CommandSpec, run_capture, run_checked},
    configuration::{ConfigContext, host_home_spec, patch_from_run_options},
    error::{HostError, Result},
    git::GitRepo,
    home::ResolvedHome,
    runtime::RuntimeKind,
};

const MAX_LINE: usize = 64 * 1024 * 1024;
const CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVITY_PROBE_INTERVAL: Duration = Duration::from_secs(5);

enum Event {
    Client(Value),
    Server(usize, Value),
    Closed(usize),
    End,
    Invalid(String),
}

struct Backend {
    child: Child,
    input: Option<ChildStdin>,
    last_activity: Instant,
    ready: bool,
    queue: Vec<Value>,
    workspace: Option<PathBuf>,
    runtime_info: PathBuf,
    copied_attachments: BTreeSet<PathBuf>,
    host_home: Option<HostHome>,
    external_activity: bool,
    next_activity_probe: Instant,
    activity_probe_warning: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostHome {
    codex: PathBuf,
    agents: PathBuf,
}

enum Pending {
    Client(Value),
    Initialize,
    Locate(Value),
    Activate(Value),
}

struct Activation {
    original: Value,
    resume: Value,
}

struct Broker {
    idle_timeout: Duration,
    filesystem: crate::adapter_fs::Filesystem,
    control_dir: PathBuf,
    common_cwd: PathBuf,
    executable: PathBuf,
    prefix: Vec<OsString>,
    arguments: Vec<OsString>,
    events: mpsc::Sender<Event>,
    backends: Vec<Backend>,
    projects: BTreeMap<PathBuf, usize>,
    threads: BTreeMap<String, usize>,
    thread_cwds: BTreeMap<String, PathBuf>,
    thread_resumes: BTreeMap<String, Value>,
    active_turns: BTreeMap<String, usize>,
    pending: BTreeMap<String, (usize, Pending)>,
    replies: BTreeMap<String, (usize, Value)>,
    initialize: Option<Value>,
    client_ready: bool,
    client_queue: Vec<Value>,
    config: Option<PathBuf>,
    config_patch: codex_start_core::ConfigPatch,
    host_codex: Option<PathBuf>,
    host_backend: Option<usize>,
    common_backend: Option<usize>,
    host_home: HostHome,
    activations: BTreeMap<usize, Vec<Activation>>,
    batches: BTreeMap<String, Batch>,
    batch_requests: BTreeMap<String, String>,
}

struct Batch {
    original: Value,
    remaining: usize,
    result: Value,
    error: Option<Value>,
}

/// Use a scratch server for account/catalog requests and select projects from RPC cwd.
pub async fn run(args: &crate::cli::AdapterArgs, config: Option<&Path>) -> Result<u8> {
    let arguments = &args.codex_args;
    for (index, argument) in arguments.iter().enumerate() {
        let listen = if argument == "--listen" {
            arguments.get(index + 1).and_then(|value| value.to_str())
        } else {
            argument
                .to_str()
                .and_then(|value| value.strip_prefix("--listen="))
        };
        if listen.is_some_and(|value| value != "stdio://") {
            return Err(HostError::Usage(
                "automatic adapter requires the app-server stdio transport".to_owned(),
            ));
        }
    }
    let scratch = tempfile::Builder::new()
        .prefix("codex-start-adapter-")
        .tempdir()
        .map_err(|source| HostError::io("adapter scratch directory", source))?;
    let run_args = crate::adapter::run_args(args.clone());
    let config_patch = patch_from_run_options(run_args.environment.as_deref(), &run_args.options);
    let context = ConfigContext::discover_at(config, scratch.path())?;
    let resolved = context.resolve(Some(config_patch.clone()))?.config;
    let settings = resolved.adapter.clone();
    let home = ResolvedHome::resolve(
        &resolved.home_name,
        &host_home_spec(&resolved.home),
        &context.paths,
    )?;
    let host_home = HostHome {
        codex: home.codex_home,
        agents: home.agents_home,
    };
    let control_dir = scratch.path().join("control");
    std::fs::create_dir(&control_dir).map_err(|source| HostError::io(&control_dir, source))?;
    #[cfg(unix)]
    std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|source| HostError::io(&control_dir, source))?;
    let (sender, mut receiver) = mpsc::channel(128);
    let mut broker = Broker {
        idle_timeout: Duration::from_secs(settings.idle_timeout_seconds),
        filesystem: crate::adapter_fs::Filesystem::default(),
        control_dir,
        common_cwd: scratch.path().to_path_buf(),
        executable: std::env::current_exe()
            .map_err(|source| HostError::io("current executable", source))?,
        prefix: launcher_arguments(&std::env::args_os().skip(1).collect::<Vec<_>>())?,
        arguments: if arguments.is_empty() {
            vec!["app-server".into()]
        } else {
            arguments.clone()
        },
        events: sender.clone(),
        backends: Vec::new(),
        projects: BTreeMap::new(),
        threads: BTreeMap::new(),
        thread_cwds: BTreeMap::new(),
        thread_resumes: BTreeMap::new(),
        active_turns: BTreeMap::new(),
        pending: BTreeMap::new(),
        replies: BTreeMap::new(),
        initialize: None,
        client_ready: false,
        client_queue: Vec::new(),
        config: config.map(Path::to_path_buf),
        config_patch,
        host_codex: find_host_codex(arguments),
        host_backend: None,
        common_backend: None,
        host_home,
        activations: BTreeMap::new(),
        batches: BTreeMap::new(),
        batch_requests: BTreeMap::new(),
    };
    if broker.host_codex.is_some() && common_home_parent(&broker.host_home).is_some() {
        if let Err(error) = broker.spawn_host(true).await {
            eprintln!(
                "codex-start-adapter: native state service is unavailable ({error}); using a container"
            );
            broker.host_codex = None;
        }
    }
    if broker.backends.is_empty() {
        let backend = broker.spawn(scratch.path(), true, None).await?;
        broker.common_backend = Some(backend);
    }
    read_client(sender);
    let result = broker.serve(&mut receiver).await;
    broker.shutdown().await;
    result
}

impl Broker {
    async fn serve(&mut self, receiver: &mut mpsc::Receiver<Event>) -> Result<u8> {
        let mut maintenance = tokio::time::interval(Duration::from_secs(1));
        loop {
            let event = tokio::select! {
                event = receiver.recv() => event,
                _ = maintenance.tick() => {
                    self.maintain().await?;
                    continue;
                },
                () = crate::app::termination_signal() => return Ok(130),
            };
            match event {
                Some(Event::Client(message)) => {
                    if let Err(error) = self.client(message.clone()).await {
                        if message.get("method").is_some() && message.get("id").is_some() {
                            output(&rpc_error(&message, &error.to_string())).await?;
                        } else {
                            return Err(error);
                        }
                    }
                }
                Some(Event::Server(backend, message)) => self.server(backend, message).await?,
                Some(Event::Closed(backend)) if self.host_backend == Some(backend) => {
                    return self.backends[backend]
                        .child
                        .wait()
                        .await
                        .map(crate::command::exit_code)
                        .map_err(|source| HostError::io("adapter server", source));
                }
                Some(Event::Closed(backend)) => self.closed(backend).await?,
                Some(Event::End) | None => return Ok(0),
                Some(Event::Invalid(message)) => return Err(HostError::Usage(message)),
            }
        }
    }

    async fn spawn(
        &mut self,
        cwd: &Path,
        common: bool,
        workspace: Option<PathBuf>,
    ) -> Result<usize> {
        let runtime_info = self
            .control_dir
            .join(format!("{}.runtime", uuid::Uuid::new_v4()));
        let mut command = tokio::process::Command::new(&self.executable);
        let mut prefix = self.prefix.clone();
        if common {
            remove_option(&mut prefix, "--environment");
            prefix.extend(["--environment".into(), "generic".into()]);
        }
        let bootstrap = self.initialize.is_none();
        let mut child = command
            .args(prefix)
            .arg("--project")
            .arg(cwd)
            .arg("--")
            .args(&self.arguments)
            .env(ATTACHMENT_RUNTIME_INFO_ENV, &runtime_info)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| HostError::io(&self.executable, source))?;
        let input = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::Runtime("missing adapter stdout".to_owned()))?;
        let backend = self.backends.len();
        self.backends.push(Backend {
            child,
            input,
            last_activity: Instant::now(),
            ready: false,
            queue: Vec::new(),
            workspace,
            runtime_info,
            copied_attachments: BTreeSet::new(),
            host_home: None,
            external_activity: false,
            next_activity_probe: Instant::now(),
            activity_probe_warning: false,
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = (&mut reader)
                    .take(u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX))
                    .read_until(b'\n', &mut line)
                    .await;
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => match decode(&line) {
                        Ok(message) => {
                            if events.send(Event::Server(backend, message)).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(Event::Invalid(error)).await;
                            return;
                        }
                    },
                }
            }
            let _ = events.send(Event::Closed(backend)).await;
        });
        if !bootstrap {
            let message = self.initialize.clone().ok_or_else(|| {
                HostError::Usage("initialize must precede project requests".to_owned())
            })?;
            self.request(backend, message, Pending::Initialize).await?;
        }
        Ok(backend)
    }

    async fn spawn_host(&mut self, bootstrap: bool) -> Result<usize> {
        let program = self
            .host_codex
            .clone()
            .ok_or_else(|| HostError::NotFound("native Codex executable".to_owned()))?;
        common_home_parent(&self.host_home).ok_or_else(|| {
            HostError::Config(
                "native skill discovery requires sibling .codex and .agents homes".to_owned(),
            )
        })?;
        let home = self.host_home.clone();
        let parent = common_home_parent(&home)
            .ok_or_else(|| HostError::Runtime("invalid native catalogue home".to_owned()))?;
        let sqlite_home = self.control_dir.join("native-sqlite-home");
        crate::paths::create_private_dir(&sqlite_home)?;
        let mut child = tokio::process::Command::new(&program)
            .args(&self.arguments)
            .env("CODEX_HOME", &home.codex)
            .env("CODEX_SQLITE_HOME", &sqlite_home)
            .env("HOME", parent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| HostError::io(&program, source))?;
        let input = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::Runtime("missing native Codex stdout".to_owned()))?;
        let backend = self.backends.len();
        self.backends.push(Backend {
            child,
            input,
            last_activity: Instant::now(),
            ready: false,
            queue: Vec::new(),
            workspace: None,
            runtime_info: PathBuf::new(),
            copied_attachments: BTreeSet::new(),
            host_home: Some(home),
            external_activity: false,
            next_activity_probe: Instant::now(),
            activity_probe_warning: false,
        });
        self.read_backend(backend, stdout);
        if !bootstrap {
            let message = self.initialize.clone().ok_or_else(|| {
                HostError::Usage("initialize must precede project requests".to_owned())
            })?;
            self.request(backend, message, Pending::Initialize).await?;
        }
        self.host_backend = Some(backend);
        Ok(backend)
    }

    fn read_backend(&self, backend: usize, stdout: tokio::process::ChildStdout) {
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = (&mut reader)
                    .take(u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX))
                    .read_until(b'\n', &mut line)
                    .await;
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(_) => match decode(&line) {
                        Ok(message) => {
                            if events.send(Event::Server(backend, message)).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = events.send(Event::Invalid(error)).await;
                            return;
                        }
                    },
                }
            }
            let _ = events.send(Event::Closed(backend)).await;
        });
    }

    fn host_state_backend(&self, message: &Value) -> Result<Option<usize>> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(backend) = self.host_backend.filter(|backend| {
            self.backends
                .get(*backend)
                .is_some_and(|backend| backend.input.is_some())
        }) else {
            return Ok(None);
        };
        if !host_catalog_method(method) {
            return Ok(None);
        }
        if method == "thread/start" && request_cwd(message).is_none() {
            return Ok(None);
        }
        let mut cwds = Vec::new();
        if let Some(cwd) = message.pointer("/params/cwd") {
            cwds.push(cwd);
        }
        if let Some(values) = message.pointer("/params/cwds").and_then(Value::as_array) {
            cwds.extend(values);
        }
        for cwd in cwds {
            let Some(cwd) = cwd.as_str() else {
                return Ok(None);
            };
            if self.home_for(cwd)? != self.host_home {
                return Ok(None);
            }
        }
        Ok(Some(backend))
    }

    async fn common(&mut self) -> Result<usize> {
        if let Some(backend) = self.common_backend.filter(|backend| {
            self.backends
                .get(*backend)
                .is_some_and(|backend| backend.input.is_some())
        }) {
            return Ok(backend);
        }
        let cwd = self.common_cwd.clone();
        let backend = self.spawn(&cwd, true, None).await?;
        self.common_backend = Some(backend);
        Ok(backend)
    }

    fn home_for(&self, cwd: &str) -> Result<HostHome> {
        let path = Path::new(cwd);
        if !path.is_absolute() {
            return Err(HostError::Usage("client cwd must be absolute".to_owned()));
        }
        let path = std::fs::canonicalize(path).map_err(|source| HostError::io(path, source))?;
        let context = ConfigContext::discover_at(self.config.as_deref(), &path)?;
        let resolved = context.resolve(Some(self.config_patch.clone()))?.config;
        let home = ResolvedHome::preview(
            &resolved.home_name,
            &host_home_spec(&resolved.home),
            &context.paths,
        )?;
        Ok(HostHome {
            codex: home.codex_home,
            agents: home.agents_home,
        })
    }

    async fn project(&mut self, cwd: &str) -> Result<usize> {
        let path = canonical_client_cwd(cwd)?;
        let root = GitRepo::discover(&path)?.map_or(path.clone(), |repo| repo.root);
        if let Some(backend) = self.projects.get(&root) {
            return Ok(*backend);
        }
        let backend = self.spawn(&path, false, Some(root.clone())).await?;
        self.projects.insert(root, backend);
        Ok(backend)
    }

    fn existing_project(&self, cwd: &str) -> Result<Option<usize>> {
        let path = canonical_client_cwd(cwd)?;
        let root = GitRepo::discover(&path)?.map_or(path, |repo| repo.root);
        Ok(self.projects.get(&root).copied())
    }

    #[allow(clippy::too_many_lines)]
    async fn client(&mut self, message: Value) -> Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            let id = message.get("id").map(Value::to_string).unwrap_or_default();
            let (backend, original) = self
                .replies
                .remove(&id)
                .ok_or_else(|| HostError::Usage("unknown server request response".to_owned()))?;
            let mut response = message;
            response["id"] = original;
            return self.send(backend, &response).await;
        };
        if method == "initialize" {
            self.initialize = Some(message.clone());
            return self
                .request(0, message.clone(), Pending::Client(message))
                .await;
        }
        if self.initialize.is_none() {
            return Err(HostError::Usage(
                "initialize must be the first request".to_owned(),
            ));
        }
        // The broker completes every backend handshake itself. Some clients,
        // including the VS Code extension, do not send this notification.
        if method == "initialized" {
            return Ok(());
        }
        // Do not answer any provider request before the client receives the
        // initialize response. VS Code sends provider requests in parallel
        // with initialize and drops their early responses.
        if !self.client_ready {
            self.client_queue.push(message);
            return Ok(());
        }
        if self.filesystem.handles(method, &message["params"]) {
            let mut filesystem = std::mem::take(&mut self.filesystem);
            let request = message.clone();
            let (filesystem_back, result) = tokio::task::spawn_blocking(move || {
                let result = filesystem.request(
                    request["method"].as_str().unwrap_or_default(),
                    &request["params"],
                );
                (filesystem, result)
            })
            .await
            .map_err(|error| HostError::Runtime(error.to_string()))?;
            self.filesystem = filesystem_back;
            return output(&match result {
                Ok(result) => json!({"id":message["id"], "result":result}),
                Err(error) => rpc_error(&message, &error.to_string()),
            })
            .await;
        }
        let thread = message
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let known = thread
            .as_deref()
            .and_then(|id| self.threads.get(id))
            .copied();
        if project_read_method(method) {
            if let Some(backend) = known.filter(|backend| !self.is_state_backend(*backend)) {
                return self.dispatch(backend, message).await;
            }
            if message
                .pointer("/params/cwds")
                .and_then(Value::as_array)
                .is_none_or(|cwds| cwds.len() <= 1)
                && let Some(cwd) = request_cwd(&message)
                && let Some(backend) = self.existing_project(&cwd)?
            {
                return self.dispatch(backend, message).await;
            }
        }
        if let Some(backend) = self.host_state_backend(&message)? {
            return self.dispatch(backend, message).await;
        }
        // A new thread has no rollout file until its first turn starts. Create it
        // in the project server so the following turn does not need to resume a
        // rollout that does not exist yet.
        if shared_state_method(method) && method != "thread/start" {
            let backend = match known {
                Some(backend) if !self.is_state_backend(backend) => backend,
                _ => self.common().await?,
            };
            return self.dispatch(backend, message).await;
        }
        if let Some(cwds) = message.pointer("/params/cwds").and_then(Value::as_array)
            && cwds.len() > 1
        {
            return self.batch(message.clone(), cwds).await;
        }
        if project_execution_method(method)
            && let Some(thread) = thread.as_deref()
            && (known.is_some_and(|backend| self.is_state_backend(backend))
                || known.is_none() && self.thread_cwds.contains_key(thread))
        {
            return self.activate_thread(thread, message).await;
        }
        // A thread stays in its original server, including turn-level cwd overrides.
        let backend = if let Some(backend) = known {
            backend
        } else if let Some(cwd) = request_cwd(&message) {
            self.project(&cwd).await?
        } else if let Some(thread) = thread.as_deref() {
            let lookup =
                json!({"method":"thread/read", "params":{"threadId":thread, "includeTurns":false}});
            let backend = self.common().await?;
            return self
                .request(backend, lookup, Pending::Locate(message))
                .await;
        } else {
            if matches!(method, "thread/start" | "command/exec") {
                return Err(HostError::Usage(
                    "automatic adapter execution requires a project cwd".to_owned(),
                ));
            }
            self.common().await?
        };
        self.dispatch(backend, message).await
    }

    async fn activate_thread(&mut self, thread: &str, original: Value) -> Result<()> {
        let Some(cwd) = self.thread_cwds.get(thread).cloned() else {
            let lookup =
                json!({"method":"thread/read", "params":{"threadId":thread, "includeTurns":false}});
            let backend = self.common().await?;
            return self
                .request(backend, lookup, Pending::Locate(original))
                .await;
        };
        self.activate_at(thread, &cwd, original).await
    }

    fn is_state_backend(&self, backend: usize) -> bool {
        self.host_backend == Some(backend) || self.common_backend == Some(backend)
    }

    async fn activate_at(&mut self, thread: &str, cwd: &Path, original: Value) -> Result<()> {
        self.release_control_writer(thread).await?;
        let backend = self.project(cwd.to_string_lossy().as_ref()).await?;
        let resume = self.thread_resumes.get(thread).map_or_else(
            || resume_request(thread, None),
            |request| resume_request(thread, Some(request)),
        );
        let activation = Activation { original, resume };
        if self.backends[backend].ready {
            self.start_activation(backend, activation).await
        } else {
            self.activations
                .entry(backend)
                .or_default()
                .push(activation);
            Ok(())
        }
    }

    async fn release_control_writer(&mut self, thread: &str) -> Result<()> {
        let Some(backend) = self.threads.get(thread).copied() else {
            return Ok(());
        };
        if self.common_backend != Some(backend) {
            return Ok(());
        }
        let pid = self.backends[backend].child.id();
        self.closed(backend).await?;
        if let Some(pid) = pid {
            let _ = tokio::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await;
        }
        match tokio::time::timeout(Duration::from_secs(20), self.backends[backend].child.wait())
            .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(source)) => Err(HostError::io("control server", source)),
            Err(_) => {
                self.backends[backend]
                    .child
                    .kill()
                    .await
                    .map_err(|source| HostError::io("control server", source))?;
                Ok(())
            }
        }
    }

    async fn start_activation(&mut self, backend: usize, activation: Activation) -> Result<()> {
        let thread = activation
            .resume
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(thread) = thread {
            self.restore_thread_attachments(backend, &thread).await?;
        }
        self.request(
            backend,
            activation.resume,
            Pending::Activate(activation.original),
        )
        .await
    }

    async fn dispatch(&mut self, backend: usize, mut message: Value) -> Result<()> {
        // The container mounts canonical host paths, not their symlink aliases.
        let normalize = |value: &mut Value| {
            if let Some(cwd) = value.as_str().filter(|cwd| Path::new(cwd).is_absolute())
                && let Ok(path) = std::fs::canonicalize(cwd)
                && let Some(path) = path.to_str()
            {
                *value = json!(path);
            }
        };
        if let Some(cwd) = message.pointer_mut("/params/cwd") {
            normalize(cwd);
        }
        if let Some(cwds) = message
            .pointer_mut("/params/cwds")
            .and_then(Value::as_array_mut)
        {
            for cwd in cwds {
                normalize(cwd);
            }
        }
        if !self.backends[backend].ready {
            self.backends[backend].queue.push(message);
            return Ok(());
        }
        self.copy_attachments(backend, &message).await?;
        if message.get("id").is_some() {
            self.request(backend, message.clone(), Pending::Client(message))
                .await
        } else {
            self.send(backend, &message).await
        }
    }

    async fn copy_attachments(&mut self, backend: usize, message: &Value) -> Result<()> {
        let Some(input) = message.pointer("/params/input").and_then(Value::as_array) else {
            return Ok(());
        };
        self.copy_attachment_paths(backend, attachment_paths(input))
            .await
    }

    async fn restore_thread_attachments(&mut self, backend: usize, thread: &str) -> Result<()> {
        let codex_home = self.host_home.codex.clone();
        let thread = thread.to_owned();
        let paths =
            tokio::task::spawn_blocking(move || historical_attachment_paths(&codex_home, &thread))
                .await
                .map_err(|error| HostError::Runtime(error.to_string()))??;
        self.copy_attachment_paths(backend, paths).await
    }

    async fn copy_attachment_paths(&mut self, backend: usize, paths: Vec<PathBuf>) -> Result<()> {
        let workspace = self.backends[backend].workspace.clone();
        let mut scheduled = BTreeSet::new();
        let mut copies = Vec::new();
        for target in paths {
            if !target.is_absolute() || target.starts_with("/home/codex") {
                continue;
            }
            if target == Path::new("/") {
                return Err(HostError::Usage(
                    "the host root cannot be used as an attachment".to_owned(),
                ));
            }
            let Ok(source) = std::fs::canonicalize(&target) else {
                continue;
            };
            if workspace
                .as_ref()
                .is_some_and(|workspace| source.starts_with(workspace))
                && source == target
            {
                continue;
            }
            if !source.is_file() && !source.is_dir() {
                return Err(HostError::Usage(format!(
                    "attachment path must be a file or directory: {}",
                    source.display()
                )));
            }
            if !self.backends[backend].copied_attachments.contains(&target)
                && scheduled.insert(target.clone())
            {
                copies.push((source, target));
            }
        }
        if copies.is_empty() {
            return Ok(());
        }
        let runtime_info = read_runtime_info(&self.backends[backend].runtime_info)?;
        let completed = copies
            .iter()
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            for (source, target) in copies {
                copy_attachment(&runtime_info, &source, &target)?;
            }
            Ok::<(), HostError>(())
        })
        .await
        .map_err(|error| HostError::Runtime(error.to_string()))??;
        self.backends[backend].copied_attachments.extend(completed);
        Ok(())
    }

    async fn request(
        &mut self,
        backend: usize,
        mut message: Value,
        pending: Pending,
    ) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        message["id"] = Value::String(id.clone());
        self.pending.insert(id.clone(), (backend, pending));
        if let Err(error) = self.send(backend, &message).await {
            self.pending.remove(&id);
            return Err(error);
        }
        Ok(())
    }

    async fn send(&mut self, backend: usize, message: &Value) -> Result<()> {
        self.backends[backend].last_activity = Instant::now();
        let input = self.backends[backend]
            .input
            .as_mut()
            .ok_or_else(|| HostError::Runtime("project server has stopped".to_owned()))?;
        let mut bytes = serde_json::to_vec(message)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        input
            .write_all(&bytes)
            .await
            .map_err(|source| HostError::io("adapter protocol input", source))
    }

    #[allow(clippy::too_many_lines)]
    async fn server(&mut self, backend: usize, mut message: Value) -> Result<()> {
        self.backends[backend].last_activity = Instant::now();
        if self.handle_host_notification(backend, &message).await? {
            return Ok(());
        }
        if message.get("method").is_some() {
            if let Some(thread) = message.pointer("/params/threadId").and_then(Value::as_str) {
                match message["method"].as_str() {
                    Some("turn/started") => {
                        self.active_turns.insert(thread.to_owned(), backend);
                    }
                    Some("turn/completed" | "thread/closed") => {
                        self.active_turns.remove(thread);
                    }
                    _ => {}
                }
            }
            if message["method"] == "thread/closed" {
                if let Some(thread) = message.pointer("/params/threadId").and_then(Value::as_str) {
                    self.threads.remove(thread);
                }
            } else {
                self.remember(backend, message.pointer("/params/thread"));
            }
            if let Some(id) = message.get("id").cloned() {
                let mapped = Value::String(uuid::Uuid::new_v4().to_string());
                self.replies.insert(mapped.to_string(), (backend, id));
                message["id"] = mapped;
            }
            return output(&message).await;
        }
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some((owner, pending)) = self.pending.remove(id) else {
            return Err(HostError::Runtime(
                "server returned an unknown response id".to_owned(),
            ));
        };
        if owner != backend {
            return Err(HostError::Runtime(
                "response came from the wrong project server".to_owned(),
            ));
        }
        match pending {
            Pending::Client(original) => {
                let initialized = original["method"] == "initialize";
                if message.get("error").is_none() {
                    if initialized {
                        self.send(backend, &json!({"method":"initialized"})).await?;
                        self.backends[backend].ready = true;
                    }
                    self.remember_cwd(message.pointer("/result/thread"));
                    if self.is_state_backend(backend)
                        && matches!(
                            original["method"].as_str(),
                            Some("thread/start" | "thread/resume" | "thread/fork")
                        )
                        && let Some(thread) =
                            message.pointer("/result/thread/id").and_then(Value::as_str)
                    {
                        self.thread_resumes
                            .insert(thread.to_owned(), original.clone());
                    }
                }
                self.rewrite_host_response(backend, &original, &mut message);
                if matches!(
                    original["method"].as_str(),
                    Some("thread/start" | "thread/resume" | "thread/fork")
                ) {
                    self.remember(backend, message.pointer("/result/thread"));
                }
                if message.get("error").is_none()
                    && let Some(thread) =
                        original.pointer("/params/threadId").and_then(Value::as_str)
                {
                    if matches!(
                        original["method"].as_str(),
                        Some("thread/unsubscribe" | "thread/archive")
                    ) {
                        self.threads.remove(thread);
                    } else if original["method"] != "thread/read" {
                        self.threads.insert(thread.to_owned(), backend);
                    }
                }
                message["id"] = original["id"].clone();
                self.respond(message).await?;
                if initialized && self.backends[backend].ready {
                    self.client_ready = true;
                    for queued in std::mem::take(&mut self.backends[backend].queue) {
                        self.dispatch(backend, queued).await?;
                    }
                    for queued in std::mem::take(&mut self.client_queue) {
                        self.client(queued).await?;
                    }
                }
                Ok(())
            }
            Pending::Initialize => {
                if message.get("error").is_some() {
                    return self.closed(backend).await;
                }
                self.send(backend, &json!({"method":"initialized"})).await?;
                self.backends[backend].ready = true;
                for activation in self.activations.remove(&backend).unwrap_or_default() {
                    self.start_activation(backend, activation).await?;
                }
                for queued in std::mem::take(&mut self.backends[backend].queue) {
                    self.dispatch(backend, queued).await?;
                }
                Ok(())
            }
            Pending::Locate(original) => {
                if let Some(cwd) = message
                    .pointer("/result/thread/cwd")
                    .and_then(Value::as_str)
                {
                    self.remember_cwd(message.pointer("/result/thread"));
                    let thread = original
                        .pointer("/params/threadId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let result = if project_execution_method(
                        original["method"].as_str().unwrap_or_default(),
                    ) {
                        match thread {
                            Some(thread) => {
                                self.activate_at(&thread, Path::new(cwd), original.clone())
                                    .await
                            }
                            None => Err(HostError::Usage(
                                "project execution requires a thread id".to_owned(),
                            )),
                        }
                    } else {
                        match self.project(cwd).await {
                            Ok(target) => self.dispatch(target, original.clone()).await,
                            Err(error) => Err(error),
                        }
                    };
                    match result {
                        Ok(()) => Ok(()),
                        Err(error) => output(&rpc_error(&original, &error.to_string())).await,
                    }
                } else {
                    output(&rpc_error(
                        &original,
                        "cannot locate this thread's project; resume it with cwd",
                    ))
                    .await
                }
            }
            Pending::Activate(original) => {
                if message.get("error").is_some() {
                    message["id"] = original["id"].clone();
                    return self.respond(message).await;
                }
                self.remember(backend, message.pointer("/result/thread"));
                self.dispatch(backend, original).await
            }
        }
    }

    async fn handle_host_notification(&self, backend: usize, message: &Value) -> Result<bool> {
        if self.backends[backend].host_home.is_none() || message.get("method").is_none() {
            return Ok(false);
        }
        if backend == 0 {
            return Ok(false);
        }
        if matches!(
            message["method"].as_str(),
            Some("skills/changed" | "hooks/changed")
        ) {
            output(message).await?;
        }
        Ok(true)
    }

    fn rewrite_host_response(&self, backend: usize, original: &Value, response: &mut Value) {
        if original["method"] == "skills/list"
            && let Some(home) = &self.backends[backend].host_home
        {
            rewrite_host_paths(response, home);
            // Codex normally preserves the catalogue-home paths. Also map the
            // shared source paths in case a linked directory was canonicalized.
            rewrite_host_paths(response, &self.host_home);
        }
    }

    async fn batch(&mut self, original: Value, cwds: &[Value]) -> Result<()> {
        let group = uuid::Uuid::new_v4().to_string();
        self.batches.insert(
            group.clone(),
            Batch {
                original: original.clone(),
                remaining: cwds.len(),
                result: json!({}),
                error: None,
            },
        );
        for cwd in cwds {
            let id = uuid::Uuid::new_v4().to_string();
            let mut request = original.clone();
            request["id"] = Value::String(id.clone());
            request["params"]["cwds"] = json!([cwd]);
            self.batch_requests
                .insert(Value::String(id).to_string(), group.clone());
            let result = match cwd.as_str() {
                Some(path) => match self.project(path).await {
                    Ok(backend) => self.dispatch(backend, request.clone()).await,
                    Err(error) => Err(error),
                },
                None => Err(HostError::Usage(
                    "cwds must contain directory strings".to_owned(),
                )),
            };
            if let Err(error) = result {
                self.respond(rpc_error(&request, &error.to_string()))
                    .await?;
            }
        }
        Ok(())
    }

    async fn respond(&mut self, response: Value) -> Result<()> {
        let id = response["id"].to_string();
        let Some(group) = self.batch_requests.remove(&id) else {
            return output(&response).await;
        };
        let batch = self
            .batches
            .get_mut(&group)
            .ok_or_else(|| HostError::Runtime("missing request batch".to_owned()))?;
        if let Some(error) = response.get("error") {
            batch.error = Some(error.clone());
        }
        if let Some(result) = response.get("result").and_then(Value::as_object) {
            for (key, value) in result {
                if let Some(items) = value.as_array() {
                    let values = batch
                        .result
                        .as_object_mut()
                        .expect("batch object")
                        .entry(key.clone())
                        .or_insert_with(|| json!([]));
                    let values = values.as_array_mut().ok_or_else(|| {
                        HostError::Runtime("incompatible list results".to_owned())
                    })?;
                    for item in items {
                        if !values.contains(item) {
                            values.push(item.clone());
                        }
                    }
                } else {
                    batch.result[key] = value.clone();
                }
            }
        }
        batch.remaining -= 1;
        if batch.remaining != 0 {
            return Ok(());
        }
        let batch = self.batches.remove(&group).expect("complete batch");
        let mut response = json!({"id":batch.original["id"]});
        if let Some(error) = batch.error {
            response["error"] = error;
        } else {
            response["result"] = batch.result;
        }
        output(&response).await
    }

    fn remember(&mut self, backend: usize, thread: Option<&Value>) {
        self.remember_cwd(thread);
        if let Some(id) = thread
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
        {
            self.threads.insert(id.to_owned(), backend);
        }
    }

    fn remember_cwd(&mut self, thread: Option<&Value>) {
        let Some(thread) = thread else {
            return;
        };
        let Some(id) = thread.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(cwd) = thread.get("cwd").and_then(Value::as_str) else {
            return;
        };
        if Path::new(cwd).is_absolute() {
            self.thread_cwds.insert(id.to_owned(), PathBuf::from(cwd));
        }
    }

    async fn closed(&mut self, backend: usize) -> Result<()> {
        if self.host_backend == Some(backend) {
            self.host_backend = None;
        }
        if self.common_backend == Some(backend) {
            self.common_backend = None;
        }
        self.backends[backend].input.take();
        self.replies.retain(|_, (owner, _)| *owner != backend);
        self.projects.retain(|_, owner| *owner != backend);
        self.threads.retain(|_, owner| *owner != backend);
        self.active_turns.retain(|_, owner| *owner != backend);
        let _ = self.backends[backend].child.try_wait();
        let ids = self
            .pending
            .iter()
            .filter(|(_, (owner, _))| *owner == backend)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some((
                _,
                Pending::Client(original) | Pending::Locate(original) | Pending::Activate(original),
            )) = self.pending.remove(&id)
            {
                self.respond(rpc_error(
                    &original,
                    "project server stopped; see stderr for the startup error",
                ))
                .await?;
            }
        }
        for activation in self.activations.remove(&backend).unwrap_or_default() {
            self.respond(rpc_error(
                &activation.original,
                "project server initialization failed; see stderr",
            ))
            .await?;
        }
        for message in std::mem::take(&mut self.backends[backend].queue) {
            if message.get("id").is_some() {
                self.respond(rpc_error(
                    &message,
                    "project server initialization failed; see stderr",
                ))
                .await?;
            }
        }
        Ok(())
    }

    async fn maintain(&mut self) -> Result<()> {
        let mut filesystem = std::mem::take(&mut self.filesystem);
        let (filesystem_back, changes) = tokio::task::spawn_blocking(move || {
            let changes = filesystem.changes();
            (filesystem, changes)
        })
        .await
        .map_err(|error| HostError::Runtime(error.to_string()))?;
        self.filesystem = filesystem_back;
        for change in changes {
            output(&change).await?;
        }
        for backend in 0..self.backends.len() {
            if self.host_backend == Some(backend) {
                continue;
            }
            if self.backends[backend].input.is_none() {
                let _ = self.backends[backend].child.try_wait();
                continue;
            }
            let control = self.common_backend == Some(backend);
            let idle_timeout = if control {
                self.idle_timeout.min(CONTROL_IDLE_TIMEOUT)
            } else {
                self.idle_timeout
            };
            if !self.backends[backend].queue.is_empty()
                || (!control && self.threads.values().any(|owner| *owner == backend))
                || self.active_turns.values().any(|owner| *owner == backend)
                || self.pending.values().any(|(owner, _)| *owner == backend)
                || self.replies.values().any(|(owner, _)| *owner == backend)
            {
                continue;
            }
            if !control && self.backends[backend].workspace.is_some() {
                let now = Instant::now();
                if !self.backends[backend].external_activity
                    && self.backends[backend].last_activity.elapsed() < idle_timeout
                {
                    continue;
                }
                if now < self.backends[backend].next_activity_probe {
                    continue;
                }
                self.backends[backend].next_activity_probe =
                    now + idle_timeout.min(ACTIVITY_PROBE_INTERVAL);
                let runtime_info = self.backends[backend].runtime_info.clone();
                let activity = tokio::task::spawn_blocking(move || {
                    let runtime = read_runtime_info(&runtime_info)?;
                    container_has_external_processes(&runtime)
                })
                .await
                .map_err(|error| HostError::Runtime(error.to_string()))?;
                match activity {
                    Ok(true) => {
                        self.backends[backend].last_activity = now;
                        self.backends[backend].external_activity = true;
                        self.backends[backend].activity_probe_warning = false;
                        continue;
                    }
                    Ok(false) => {
                        let was_external = self.backends[backend].external_activity;
                        self.backends[backend].external_activity = false;
                        self.backends[backend].activity_probe_warning = false;
                        if was_external {
                            self.backends[backend].last_activity = now;
                            continue;
                        }
                    }
                    Err(error) => {
                        self.backends[backend].last_activity = now;
                        self.backends[backend].external_activity = true;
                        if !self.backends[backend].activity_probe_warning {
                            let workspace =
                                self.backends[backend].workspace.as_deref().map_or_else(
                                    || "<unknown>".to_owned(),
                                    |path| path.display().to_string(),
                                );
                            eprintln!(
                                "codex-start-adapter: preserving idle project container for {workspace}: cannot inspect its processes: {error}"
                            );
                            self.backends[backend].activity_probe_warning = true;
                        }
                        continue;
                    }
                }
            }
            if self.backends[backend].last_activity.elapsed() < idle_timeout {
                continue;
            }
            self.closed(backend).await?;
            // The child still owns its cleanup. Reap it on EOF or at adapter shutdown.
            if let Some(pid) = self.backends[backend].child.id() {
                let _ = tokio::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .await;
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        let mut tasks = Vec::new();
        for mut backend in self.backends.drain(..) {
            backend.input.take();
            tasks.push(tokio::spawn(async move {
                if let Some(pid) = backend.child.id() {
                    // SIGTERM lets the foreground launcher stop and remove its container.
                    let _ = tokio::process::Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .status()
                        .await;
                }
                if tokio::time::timeout(Duration::from_secs(20), backend.child.wait())
                    .await
                    .is_err()
                {
                    let _ = backend.child.kill().await;
                }
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

fn attachment_paths(input: &[Value]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("localImage" | "localAudio" | "mention" | "skill") => {
                if let Some(path) = item.get("path").and_then(Value::as_str) {
                    paths.push(normalized_attachment_path(path));
                }
            }
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    paths.extend(text_attachment_paths(text));
                }
            }
            _ => {}
        }
    }
    paths
}

fn historical_attachment_paths(codex_home: &Path, thread: &str) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for rollout in thread_rollouts(codex_home, thread)? {
        let file =
            std::fs::File::open(&rollout).map_err(|source| HostError::io(&rollout, source))?;
        let mut reader = std::io::BufReader::new(file);
        loop {
            let mut line = Vec::new();
            let count = (&mut reader)
                .take(u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX))
                .read_until(b'\n', &mut line)
                .map_err(|source| HostError::io(&rollout, source))?;
            if count == 0 {
                break;
            }
            if line.len() > MAX_LINE {
                return Err(HostError::Runtime(format!(
                    "rollout line exceeds 64 MiB: {}",
                    rollout.display()
                )));
            }
            let Ok(event) = serde_json::from_slice::<Value>(&line) else {
                // A live rollout can end with one incomplete line.
                continue;
            };
            historical_event_attachment_paths(&event, &mut paths);
        }
    }
    Ok(paths.into_iter().collect())
}

fn thread_rollouts(codex_home: &Path, thread: &str) -> Result<Vec<PathBuf>> {
    let suffix = format!("-{thread}.jsonl");
    let mut directories = ["sessions", "archived_sessions"]
        .into_iter()
        .map(|name| codex_home.join(name))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    let mut rollouts = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in
            std::fs::read_dir(&directory).map_err(|source| HostError::io(&directory, source))?
        {
            let entry = entry.map_err(|source| HostError::io(&directory, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| HostError::io(entry.path(), source))?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(&suffix))
            {
                rollouts.push(entry.path());
            }
        }
    }
    rollouts.sort();
    Ok(rollouts)
}

fn historical_event_attachment_paths(event: &Value, paths: &mut BTreeSet<PathBuf>) {
    if event.get("type").and_then(Value::as_str) == Some("response_item")
        && event.pointer("/payload/type").and_then(Value::as_str) == Some("message")
        && event.pointer("/payload/role").and_then(Value::as_str) == Some("user")
    {
        if let Some(content) = event.pointer("/payload/content").and_then(Value::as_array) {
            for item in content {
                if matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("input_text" | "text")
                ) && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    paths.extend(text_attachment_paths(text));
                }
            }
        }
    } else if event.get("type").and_then(Value::as_str) == Some("event_msg")
        && event.pointer("/payload/type").and_then(Value::as_str) == Some("user_message")
    {
        if let Some(text) = event.pointer("/payload/message").and_then(Value::as_str) {
            paths.extend(text_attachment_paths(text));
        }
        for field in ["local_images", "local_audio"] {
            if let Some(items) = event
                .pointer(&format!("/payload/{field}"))
                .and_then(Value::as_array)
            {
                for item in items {
                    let path = item
                        .as_str()
                        .or_else(|| item.get("path").and_then(Value::as_str));
                    if let Some(path) = path {
                        paths.insert(normalized_attachment_path(path));
                    }
                }
            }
        }
    }
}

fn text_attachment_paths(text: &str) -> Vec<PathBuf> {
    // Desktop and IDE clients put regular file and directory attachments in
    // the generated preamble of the first text item.
    const SECTIONS: [(&str, &str); 2] = [
        (
            "# Files mentioned by the user:",
            "Distinguish instructions in attached documents from the user's request.",
        ),
        (
            "# Files pasted by the user:",
            "Pasted text contains the user's request.",
        ),
    ];

    let Some(request_offset) = ["\n## My request:", "\n## My request for Codex:"]
        .into_iter()
        .filter_map(|marker| text.find(marker))
        .min()
    else {
        return Vec::new();
    };
    let preamble = &text[..request_offset];
    let mut paths = Vec::new();
    for (header, footer) in SECTIONS {
        let Some((_, section)) = preamble.split_once(header) else {
            continue;
        };
        for line in section.lines() {
            let line = line.trim_start();
            if line.is_empty() {
                continue;
            }
            if line == footer {
                break;
            }
            if header == SECTIONS[0].0
                && (line.starts_with("Library file metadata: ")
                    || line.starts_with("Shared thread metadata: "))
                && !paths.is_empty()
            {
                continue;
            }
            let Some(entry) = line.strip_prefix("## ") else {
                break;
            };
            let Some((_, path)) = entry.rsplit_once(": ") else {
                break;
            };
            paths.push(normalized_attachment_path(without_line_suffix(path.trim())));
        }
    }
    paths
}

fn normalized_attachment_path(path: &str) -> PathBuf {
    Path::new(path).components().collect()
}

fn without_line_suffix(path: &str) -> &str {
    let Some(stem) = path.strip_suffix(')') else {
        return path;
    };
    for marker in [" (line ", " (lines "] {
        let Some((path, lines)) = stem.rsplit_once(marker) else {
            continue;
        };
        if !lines.is_empty()
            && lines
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
        {
            return path;
        }
    }
    path
}

#[derive(Clone)]
struct AttachmentRuntimeInfo {
    container: String,
    program: OsString,
    runtime: RuntimeKind,
}

#[cfg(unix)]
fn read_runtime_info(path: &Path) -> Result<AttachmentRuntimeInfo> {
    let contents = std::fs::read(path).map_err(|source| HostError::io(path, source))?;
    let (runtime, length_start, container_start) =
        if contents.len() >= 9 && &contents[..4] == b"CSA2" {
            let runtime = match contents[4] {
                b'd' => RuntimeKind::Docker,
                b'p' => RuntimeKind::Podman,
                _ => {
                    return Err(HostError::Runtime(
                        "invalid adapter runtime kind".to_owned(),
                    ));
                }
            };
            (Some(runtime), 5_usize, 9_usize)
        } else if contents.len() >= 8 && &contents[..4] == b"CSAI" {
            (None, 4_usize, 8_usize)
        } else {
            return Err(HostError::Runtime(
                "invalid adapter attachment runtime information".to_owned(),
            ));
        };
    let length =
        u32::from_be_bytes(contents[length_start..length_start + 4].try_into().unwrap()) as usize;
    let end = container_start.checked_add(length).ok_or_else(|| {
        HostError::Runtime("invalid adapter attachment runtime information".to_owned())
    })?;
    if end > contents.len() || end == contents.len() {
        return Err(HostError::Runtime(
            "invalid adapter attachment runtime information".to_owned(),
        ));
    }
    let container = std::str::from_utf8(&contents[container_start..end])
        .map_err(|_| HostError::Runtime("invalid adapter container name".to_owned()))?
        .to_owned();
    let program = OsString::from_vec(contents[end..].to_vec());
    let runtime = runtime.unwrap_or_else(|| legacy_runtime_kind(&program));
    Ok(AttachmentRuntimeInfo {
        container,
        program,
        runtime,
    })
}

#[cfg(unix)]
fn legacy_runtime_kind(program: &std::ffi::OsStr) -> RuntimeKind {
    let name = Path::new(program)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    if name.contains("podman") {
        RuntimeKind::Podman
    } else {
        RuntimeKind::Docker
    }
}

#[cfg(not(unix))]
fn read_runtime_info(_path: &Path) -> Result<AttachmentRuntimeInfo> {
    Err(HostError::Usage(
        "adapter attachments require macOS or Linux".to_owned(),
    ))
}

#[derive(Debug, Eq, PartialEq)]
struct ContainerProcess {
    pid: u64,
    ppid: u64,
    state: String,
    args: String,
}

fn container_has_external_processes(info: &AttachmentRuntimeInfo) -> Result<bool> {
    let command = match info.runtime {
        RuntimeKind::Auto | RuntimeKind::Docker => CommandSpec::new(info.program.clone()).args([
            OsString::from("top"),
            OsString::from(&info.container),
            OsString::from("-eo"),
            OsString::from("pid,ppid,stat,args"),
        ]),
        RuntimeKind::Podman => CommandSpec::new(info.program.clone()).args([
            OsString::from("top"),
            OsString::from(&info.container),
            OsString::from("pid"),
            OsString::from("ppid"),
            OsString::from("state"),
            OsString::from("args"),
        ]),
    };
    let output = run_checked(&command)?;
    let processes = parse_container_processes(&String::from_utf8_lossy(&output.stdout))?;
    Ok(has_external_process(&processes))
}

fn parse_container_processes(output: &str) -> Result<Vec<ContainerProcess>> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let header = lines
        .next()
        .ok_or_else(|| HostError::Runtime("container process list is empty".to_owned()))?;
    let header = header.to_ascii_uppercase();
    if !header.contains("PID") || !header.contains("PPID") {
        return Err(HostError::Runtime(
            "container process list has an unknown format".to_owned(),
        ));
    }
    let mut processes = Vec::new();
    for line in lines {
        let mut rest = line;
        let pid = take_process_field(&mut rest)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| HostError::Runtime("container process has an invalid PID".to_owned()))?;
        let parent_pid = take_process_field(&mut rest)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                HostError::Runtime("container process has an invalid PPID".to_owned())
            })?;
        let state = take_process_field(&mut rest)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| HostError::Runtime("container process has no state".to_owned()))?;
        let args = rest.trim_start();
        if args.is_empty() {
            return Err(HostError::Runtime(
                "container process has no command".to_owned(),
            ));
        }
        processes.push(ContainerProcess {
            pid,
            ppid: parent_pid,
            state: state.to_owned(),
            args: args.to_owned(),
        });
    }
    if processes.is_empty() {
        return Err(HostError::Runtime(
            "container process list has no processes".to_owned(),
        ));
    }
    Ok(processes)
}

fn take_process_field<'a>(input: &mut &'a str) -> Option<&'a str> {
    *input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let (field, rest) = input.split_at(end);
    *input = rest;
    Some(field)
}

fn has_external_process(processes: &[ContainerProcess]) -> bool {
    let live = processes
        .iter()
        .filter(|process| !process.state.starts_with('Z'))
        .collect::<Vec<_>>();
    let pids = live
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    let roots = live
        .iter()
        .copied()
        .filter(|process| !pids.contains(&process.ppid))
        .collect::<Vec<_>>();
    let primary = roots
        .iter()
        .copied()
        .find(|process| is_codex_app_server(&process.args))
        .or_else(|| (roots.len() == 1).then_some(roots[0]));
    let Some(primary) = primary else {
        return true;
    };
    live.into_iter().any(|process| {
        process.pid != primary.pid
            && !is_codex_start_service(&process.args)
            && !(process.ppid == primary.pid && is_codex_app_server(&process.args))
    })
}

fn is_codex_app_server(args: &str) -> bool {
    let words = args.split_ascii_whitespace().collect::<Vec<_>>();
    words.contains(&"app-server")
        && words
            .iter()
            .take(3)
            .any(|word| Path::new(word).file_name() == Some(std::ffi::OsStr::new("codex")))
}

fn is_codex_start_service(args: &str) -> bool {
    let mut words = args.split_ascii_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    if program != "/usr/local/bin/codex-start-init" {
        return false;
    }
    matches!(
        words.next(),
        Some(
            "http-proxy"
                | "connect-bridge"
                | "tcp-forward"
                | "tcp-bridge"
                | "unix-bridge"
                | "oauth-target"
        )
    )
}

#[cfg(unix)]
fn copy_attachment(info: &AttachmentRuntimeInfo, source: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().ok_or_else(|| {
        HostError::Usage(format!(
            "attachment path has no parent: {}",
            target.display()
        ))
    })?;
    for predicate in ["-e", "-L"] {
        let output = run_capture(&CommandSpec::new(info.program.clone()).args([
            OsString::from("exec"),
            OsString::from("--user"),
            OsString::from("0"),
            OsString::from(&info.container),
            OsString::from("test"),
            OsString::from("!"),
            OsString::from(predicate),
            target.as_os_str().to_owned(),
        ]))?;
        if !output.status.success() {
            return Err(HostError::Usage(format!(
                "attachment destination already exists in the container: {}",
                target.display()
            )));
        }
    }
    run_checked(&CommandSpec::new(info.program.clone()).args([
        OsString::from("exec"),
        OsString::from("--user"),
        OsString::from("0"),
        OsString::from(&info.container),
        OsString::from("mkdir"),
        OsString::from("-p"),
        OsString::from("--"),
        parent.as_os_str().to_owned(),
    ]))?;
    run_checked(&CommandSpec::new(info.program.clone()).args([
        OsString::from("cp"),
        source.as_os_str().to_owned(),
        OsString::from(format!("{}:{}", info.container, target.display())),
    ]))?;
    run_checked(&CommandSpec::new(info.program.clone()).args([
        OsString::from("exec"),
        OsString::from("--user"),
        OsString::from("0"),
        OsString::from(&info.container),
        OsString::from("chmod"),
        OsString::from("-R"),
        OsString::from("a+rX"),
        OsString::from("--"),
        target.as_os_str().to_owned(),
    ]))?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_attachment(_info: &AttachmentRuntimeInfo, _source: &Path, _target: &Path) -> Result<()> {
    Err(HostError::Usage(
        "adapter attachments require macOS or Linux".to_owned(),
    ))
}

fn request_cwd(message: &Value) -> Option<String> {
    if let Some(cwd) = message
        .pointer("/params/cwd")
        .and_then(Value::as_str)
        .or_else(|| message.pointer("/params/cwds/0").and_then(Value::as_str))
        .filter(|cwd| !cwd.is_empty())
    {
        return Some(cwd.to_owned());
    }
    None
}

fn canonical_client_cwd(cwd: &str) -> Result<PathBuf> {
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err(HostError::Usage("client cwd must be absolute".to_owned()));
    }
    let path = std::fs::canonicalize(path).map_err(|source| HostError::io(path, source))?;
    if !path.is_dir() {
        return Err(HostError::Usage(
            "client cwd must be a directory".to_owned(),
        ));
    }
    Ok(path)
}

fn host_catalog_method(method: &str) -> bool {
    matches!(
        method,
        "server/diagnostics"
            | "userVerification/status"
            | "skills/list"
            | "hooks/list"
            | "plugin/list"
            | "plugin/search"
            | "plugin/installed"
            | "plugin/read"
            | "plugin/skill/read"
            | "plugin/share/list"
            | "marketplace/add"
            | "app/read"
            | "app/list"
            | "app/installed"
            | "model/list"
            | "modelProvider/capabilities/read"
            | "experimentalFeature/list"
            | "permissionProfile/list"
            | "collaborationMode/list"
            | "remoteControl/status/read"
            | "mcpServerStatus/list"
            | "config/read"
            | "configRequirements/read"
            | "externalAgentConfig/detect"
            | "externalAgentConfig/import/readHistories"
            | "windowsSandbox/readiness"
            | "account/read"
            | "getAuthStatus"
            | "account/rateLimits/read"
            | "account/usage/read"
            | "account/workspaceMessages/read"
    )
}

fn shared_state_method(method: &str) -> bool {
    matches!(
        method,
        "thread/start"
            | "thread/resume"
            | "thread/fork"
            | "thread/goal/get"
            | "thread/queue/list"
            | "thread/backgroundTerminals/list"
            | "thread/list"
            | "thread/search"
            | "thread/searchOccurrences"
            | "thread/loaded/list"
            | "thread/read"
            | "thread/turns/list"
            | "thread/items/list"
            | "thread/timeline/list"
            | "thread/realtime/listVoices"
            | "project/list"
            | "project/read"
            | "threadSection/list"
    )
}

fn project_read_method(method: &str) -> bool {
    host_catalog_method(method)
        || matches!(
            method,
            "thread/goal/get"
                | "thread/queue/list"
                | "thread/backgroundTerminals/list"
                | "thread/list"
                | "thread/search"
                | "thread/searchOccurrences"
                | "thread/loaded/list"
                | "thread/read"
                | "thread/turns/list"
                | "thread/items/list"
                | "thread/timeline/list"
                | "thread/realtime/listVoices"
                | "project/list"
                | "project/read"
                | "threadSection/list"
        )
}

fn project_execution_method(method: &str) -> bool {
    matches!(
        method,
        "turn/start"
            | "thread/compact/start"
            | "thread/shellCommand"
            | "thread/queue/start"
            | "thread/realtime/start"
            | "review/start"
    )
}

fn resume_request(thread: &str, template: Option<&Value>) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_owned(), Value::String(thread.to_owned()));
    params.insert("excludeTurns".to_owned(), Value::Bool(true));
    if let Some(source) = template.and_then(|request| request["params"].as_object()) {
        for key in [
            "approvalPolicy",
            "approvalsReviewer",
            "baseInstructions",
            "config",
            "cwd",
            "developerInstructions",
            "model",
            "modelProvider",
            "permissions",
            "personality",
            "runtimeWorkspaceRoots",
            "sandbox",
            "serviceTier",
        ] {
            if let Some(value) = source.get(key) {
                params.insert(key.to_owned(), value.clone());
            }
        }
    }
    json!({"method":"thread/resume", "params":params})
}

fn common_home_parent(home: &HostHome) -> Option<&Path> {
    let parent = home.codex.parent()?;
    (home.codex.file_name() == Some(std::ffi::OsStr::new(".codex"))
        && home.agents == parent.join(".agents"))
    .then_some(parent)
}

fn find_host_codex(arguments: &[OsString]) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("CODEX_START_ADAPTER_HOST_CODEX") {
        if value.is_empty() || value == "off" {
            return None;
        }
        return executable_file(PathBuf::from(value));
    }
    if let Some(resources) = std::env::var_os("CODEX_ELECTRON_RESOURCES_PATH")
        && let Some(path) = executable_file(PathBuf::from(resources).join("codex"))
    {
        return Some(path);
    }
    if let Some(path) = client_codex_from_arguments(arguments) {
        return Some(path);
    }
    #[cfg(target_os = "macos")]
    for path in [
        "/Applications/ChatGPT.app/Contents/Resources/codex",
        "/Applications/Codex.app/Contents/Resources/codex",
    ] {
        if let Some(path) = executable_file(PathBuf::from(path)) {
            return Some(path);
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .filter(|directory| directory.is_absolute())
            .find_map(|directory| {
                executable_file(directory.join(if cfg!(windows) { "codex.exe" } else { "codex" }))
            })
    })
}

fn client_codex_from_arguments(arguments: &[OsString]) -> Option<PathBuf> {
    for argument in arguments {
        let text = argument.to_string_lossy();
        let Some(marker) = text.find(".app/Contents/Resources/") else {
            continue;
        };
        let Some(start) = text[..marker]
            .rfind(['\"', '='])
            .map(|position| position + 1)
            .or_else(|| text[..marker].rfind(' '))
        else {
            continue;
        };
        let end = marker + ".app/Contents/Resources".len();
        if let Some(path) = executable_file(PathBuf::from(&text[start..end]).join("codex")) {
            return Some(path);
        }
    }
    None
}

fn executable_file(path: PathBuf) -> Option<PathBuf> {
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    Some(path)
}

fn rewrite_host_paths(value: &mut Value, home: &HostHome) {
    match value {
        Value::String(text) => {
            for (source, target) in [
                (&home.codex, Path::new("/home/codex/.codex")),
                (&home.agents, Path::new("/home/codex/.agents")),
            ] {
                let path = Path::new(text);
                if let Ok(suffix) = path.strip_prefix(source) {
                    *text = target.join(suffix).to_string_lossy().into_owned();
                    break;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_host_paths(value, home);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                rewrite_host_paths(value, home);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn remove_option(arguments: &mut Vec<OsString>, option: &str) {
    let mut result = Vec::new();
    let mut input = std::mem::take(arguments).into_iter();
    while let Some(argument) = input.next() {
        if argument == option {
            input.next();
        } else if !argument
            .to_string_lossy()
            .starts_with(&format!("{option}="))
        {
            result.push(argument);
        }
    }
    *arguments = result;
}

fn read_client(events: mpsc::Sender<Event>) {
    // A detached native thread avoids keeping Tokio alive on an uncancellable stdin read.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = Vec::new();
            let result = (&mut reader)
                .take(u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX))
                .read_until(b'\n', &mut line);
            let event = match result {
                Ok(0) => Event::End,
                Ok(_) => match decode(&line) {
                    Ok(message) => Event::Client(message),
                    Err(error) => Event::Invalid(error),
                },
                Err(error) => Event::Invalid(format!("cannot read client protocol: {error}")),
            };
            let end = matches!(event, Event::End | Event::Invalid(_));
            if events.blocking_send(event).is_err() || end {
                return;
            }
        }
    });
}

fn decode(bytes: &[u8]) -> std::result::Result<Value, String> {
    if bytes.len() > MAX_LINE {
        return Err("app-server message exceeds 64 MiB".to_owned());
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid app-server JSON: {error}"))?;
    if !value.is_object() {
        return Err("app-server messages must be JSON objects".to_owned());
    }
    Ok(value)
}

fn rpc_error(request: &Value, message: &str) -> Value {
    json!({"id":request["id"], "error":{"code":-32000,"message":message}})
}

async fn output(message: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|error| HostError::Serialization(error.to_string()))?;
    bytes.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(&bytes)
        .await
        .map_err(|source| HostError::io("adapter protocol output", source))?;
    stdout
        .flush()
        .await
        .map_err(|source| HostError::io("adapter protocol output", source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_attachment_without_legacy_footer_keeps_absolute_path() {
        let path = "/Users/example/.codex/attachments/id/pasted-text.txt";
        let text = format!(
            "\n# Files pasted by the user:\n\n## pasted-text.txt: {path}\n\n## My request:\nReview it.\n"
        );

        assert_eq!(text_attachment_paths(&text), vec![PathBuf::from(path)]);
    }

    #[test]
    fn historical_attachments_are_read_from_a_thread_rollout() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join(".codex");
        let sessions = codex_home.join("sessions/2026/09/07");
        std::fs::create_dir_all(&sessions).unwrap();
        let pasted = root.path().join("pasted.bin");
        let folder = root.path().join("folder");
        let audio = root.path().join("audio.wav");
        std::fs::write(&pasted, [0, 1, 254, 255]).unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(&audio, b"audio").unwrap();
        let text = format!(
            "\n# Files pasted by the user:\n\n## pasted.bin: {}\n\n## folder: {}/\n\n## My request:\nReview them.\n",
            pasted.display(),
            folder.display()
        );
        let ignored = format!(
            "\n# Files pasted by the user:\n\n## ignored: {}\n\n## My request:\nIgnore it.\n",
            root.path().join("ignored").display()
        );
        let rollout = sessions.join("rollout-2026-09-07T00-00-00-saved-thread.jsonl");
        std::fs::write(
            &rollout,
            format!(
                "{}\n{}\n{}\n{{incomplete",
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":ignored}]}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"plain text","local_images":[],"local_audio":[audio]}})
            ),
        )
        .unwrap();

        assert_eq!(
            historical_attachment_paths(&codex_home, "saved-thread")
                .unwrap()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([audio, folder, pasted])
        );
    }

    const BASELINE_PROCESSES: &str = "\
PID PPID STAT COMMAND
100 1 S {MainThread} node /home/codex/.local/bin/codex -c sandbox_mode=\"danger-full-access\" app-server
101 100 S /home/codex/.local/lib/node_modules/@openai/codex/vendor/bin/codex -c sandbox_mode=\"danger-full-access\" app-server
102 100 S /usr/local/bin/codex-start-init unix-bridge --listen /home/codex/.gnupg/S.gpg-agent
";

    #[test]
    fn finds_the_client_codex_and_maps_managed_home_paths() {
        let root = tempfile::tempdir().unwrap();
        let resources = root.path().join("Client.app/Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        let executable = resources.join("codex");
        std::fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let arguments = vec![OsString::from(format!(
            "mcp={{command=\"{}/plugins/server\"}}",
            resources.display()
        ))];
        assert_eq!(client_codex_from_arguments(&arguments), Some(executable));

        let home = HostHome {
            codex: root.path().join("home/.codex"),
            agents: root.path().join("home/.agents"),
        };
        let mut response = json!({
            "path": home.codex.join("skills/example/SKILL.md"),
            "icon": home.agents.join("skills/example/icon.png"),
            "project": root.path().join("project/SKILL.md"),
        });
        rewrite_host_paths(&mut response, &home);
        assert_eq!(
            response["path"],
            "/home/codex/.codex/skills/example/SKILL.md"
        );
        assert_eq!(
            response["icon"],
            "/home/codex/.agents/skills/example/icon.png"
        );
        assert_eq!(
            response["project"],
            json!(root.path().join("project/SKILL.md"))
        );
    }

    #[test]
    fn native_sqlite_home_is_separate_from_the_catalog_home() {
        let root = tempfile::tempdir().unwrap();
        let home = HostHome {
            codex: root.path().join("shared/.codex"),
            agents: root.path().join("shared/.agents"),
        };
        let sqlite_home = root.path().join("control/native-sqlite-home");
        assert_ne!(sqlite_home, home.codex);
        assert!(!sqlite_home.starts_with(&home.codex));
    }

    #[test]
    fn process_probe_ignores_codex_services_and_zombies() {
        let processes =
            parse_container_processes(&format!("{BASELINE_PROCESSES}200 100 Z [bash] <defunct>\n"))
                .unwrap();
        assert!(!has_external_process(&processes));
    }

    #[test]
    fn process_probe_keeps_tcp_udp_and_sleeping_processes() {
        for command in [
            "python3 -m http.server 8080",
            "nc -u -l 9000",
            "sleep 86400",
            "node server.js",
        ] {
            let processes =
                parse_container_processes(&format!("{BASELINE_PROCESSES}200 100 S {command}\n"))
                    .unwrap();
            assert!(has_external_process(&processes), "{command}");
        }
    }

    #[test]
    fn process_probe_parses_podman_descriptors() {
        let processes = parse_container_processes(
            "PID PPID STATE ARGS\n\
             1 0 S codex app-server\n\
             9 1 S /usr/local/bin/codex-start-init tcp-forward --listen 127.0.0.1:5432\n",
        )
        .unwrap();
        assert!(!has_external_process(&processes));
    }
}
