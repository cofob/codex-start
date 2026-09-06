//! Route one app-server connection to project-specific container processes.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};

use crate::{
    adapter::launcher_arguments,
    error::{HostError, Result},
    git::GitRepo,
};

const MAX_LINE: usize = 64 * 1024 * 1024;

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
}

enum Pending {
    Client(Value),
    Initialize,
    Locate(Value),
}

struct Broker {
    idle_timeout: Duration,
    filesystem: crate::adapter_fs::Filesystem,
    executable: PathBuf,
    prefix: Vec<OsString>,
    arguments: Vec<OsString>,
    events: mpsc::Sender<Event>,
    backends: Vec<Backend>,
    projects: BTreeMap<PathBuf, usize>,
    threads: BTreeMap<String, usize>,
    active_turns: BTreeMap<String, usize>,
    pending: BTreeMap<String, (usize, Pending)>,
    replies: BTreeMap<String, (usize, Value)>,
    initialize: Option<Value>,
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
pub async fn run(arguments: &[OsString], config: Option<&Path>) -> Result<u8> {
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
    let settings = crate::configuration::ConfigContext::discover_at(config, scratch.path())?
        .resolve(None)?
        .config
        .adapter;
    let (sender, mut receiver) = mpsc::channel(128);
    let mut broker = Broker {
        idle_timeout: Duration::from_secs(settings.idle_timeout_seconds),
        filesystem: crate::adapter_fs::Filesystem::default(),
        executable: std::env::current_exe()
            .map_err(|source| HostError::io("current executable", source))?,
        prefix: launcher_arguments(&std::env::args_os().skip(1).collect::<Vec<_>>())?,
        arguments: if arguments.is_empty() {
            vec!["app-server".into()]
        } else {
            arguments.to_vec()
        },
        events: sender.clone(),
        backends: Vec::new(),
        projects: BTreeMap::new(),
        threads: BTreeMap::new(),
        active_turns: BTreeMap::new(),
        pending: BTreeMap::new(),
        replies: BTreeMap::new(),
        initialize: None,
        batches: BTreeMap::new(),
        batch_requests: BTreeMap::new(),
    };
    broker.spawn(scratch.path(), true).await?;
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
                Some(Event::Closed(0)) => {
                    return self.backends[0]
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

    async fn spawn(&mut self, cwd: &Path, bootstrap: bool) -> Result<usize> {
        let mut command = tokio::process::Command::new(&self.executable);
        let mut prefix = self.prefix.clone();
        if bootstrap {
            remove_option(&mut prefix, "--environment");
            prefix.extend(["--environment".into(), "generic".into()]);
        }
        let mut child = command
            .args(prefix)
            .arg("--project")
            .arg(cwd)
            .arg("--")
            .args(&self.arguments)
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
            ready: bootstrap,
            queue: Vec::new(),
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

    async fn project(&mut self, cwd: &str) -> Result<usize> {
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
        let root = GitRepo::discover(&path)?.map_or(path.clone(), |repo| repo.root);
        if let Some(backend) = self.projects.get(&root) {
            return Ok(*backend);
        }
        let backend = self.spawn(&path, false).await?;
        self.projects.insert(root, backend);
        Ok(backend)
    }

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
        if let Some(cwds) = message.pointer("/params/cwds").and_then(Value::as_array)
            && cwds.len() > 1
        {
            return self.batch(message.clone(), cwds).await;
        }
        let thread = message.pointer("/params/threadId").and_then(Value::as_str);
        let known = thread.and_then(|id| self.threads.get(id)).copied();
        // A thread stays in its original server, including turn-level cwd overrides.
        let backend = if let Some(backend) = known {
            backend
        } else if let Some(cwd) = request_cwd(&message) {
            self.project(&cwd).await?
        } else if let Some(thread) = thread {
            let lookup =
                json!({"method":"thread/read", "params":{"threadId":thread, "includeTurns":false}});
            return self.request(0, lookup, Pending::Locate(message)).await;
        } else {
            if matches!(method, "thread/start" | "command/exec") {
                return Err(HostError::Usage(
                    "automatic adapter execution requires a project cwd".to_owned(),
                ));
            }
            0
        };
        self.dispatch(backend, message).await
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
        if message.get("id").is_some() {
            self.request(backend, message.clone(), Pending::Client(message))
                .await
        } else {
            self.send(backend, &message).await
        }
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

    async fn server(&mut self, backend: usize, mut message: Value) -> Result<()> {
        self.backends[backend].last_activity = Instant::now();
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
                self.respond(message).await
            }
            Pending::Initialize => {
                if message.get("error").is_some() {
                    return self.closed(backend).await;
                }
                self.send(backend, &json!({"method":"initialized"})).await?;
                self.backends[backend].ready = true;
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
                    match self.project(cwd).await {
                        Ok(target) => self.dispatch(target, original).await,
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
        if let Some(id) = thread
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
        {
            self.threads.insert(id.to_owned(), backend);
        }
    }

    async fn closed(&mut self, backend: usize) -> Result<()> {
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
            if let Some((_, Pending::Client(original) | Pending::Locate(original))) =
                self.pending.remove(&id)
            {
                self.respond(rpc_error(
                    &original,
                    "project server stopped; see stderr for the startup error",
                ))
                .await?;
            }
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
        for backend in 1..self.backends.len() {
            if self.backends[backend].input.is_none() {
                let _ = self.backends[backend].child.try_wait();
                continue;
            }
            if self.backends[backend].last_activity.elapsed() < self.idle_timeout
                || !self.backends[backend].queue.is_empty()
                || self.threads.values().any(|owner| *owner == backend)
                || self.active_turns.values().any(|owner| *owner == backend)
                || self.pending.values().any(|(owner, _)| *owner == backend)
                || self.replies.values().any(|(owner, _)| *owner == backend)
            {
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
