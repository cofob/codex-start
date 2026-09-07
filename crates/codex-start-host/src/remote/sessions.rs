//! Connections to existing app-server instances and session-scoped RPC routing.

use super::{error, state::State};
use crate::{
    error::Result,
    session::{APP_SERVER_SOCKET, SessionKind, SessionStore},
};
use codex_start_remote::{SessionEndpoint, SessionId, SessionInfo};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, oneshot},
};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub struct Sessions {
    backends: BTreeMap<String, Arc<Backend>>,
}
pub struct Backend {
    pub info: SessionInfo,
    endpoint: SessionEndpoint,
    outgoing: mpsc::Sender<Value>,
    pending: Mutex<BTreeMap<String, oneshot::Sender<Value>>>,
    next: AtomicU64,
    approvals: Mutex<BTreeMap<String, bool>>,
    observed: Mutex<std::collections::BTreeSet<String>>,
}

struct ProcessStream {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
}

struct PendingGuard<'a> {
    pending: &'a Mutex<BTreeMap<String, oneshot::Sender<Value>>>,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

impl AsyncRead for ProcessStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.output).poll_read(cx, buf)
    }
}
impl AsyncWrite for ProcessStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.input).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.input).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.input).poll_shutdown(cx)
    }
}
impl Drop for ProcessStream {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub async fn connect_endpoint(
    endpoint: &SessionEndpoint,
) -> Result<tokio_tungstenite::WebSocketStream<codex_start_transport::Stream>> {
    let mut child = tokio::process::Command::new(&endpoint.program)
        .args(&endpoint.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(error)?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| error("missing proxy stdin"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| error("missing proxy stdout"))?;
    let stream: codex_start_transport::Stream = Box::new(ProcessStream {
        child,
        input,
        output,
    });
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(codex_start_remote::MAX_MESSAGE_BYTES));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config)),
    )
    .await
    .map_err(error)?
    .map_err(error)?;
    Ok(socket)
}

impl Backend {
    fn track_approval(&self, message: &Value) {
        if let Ok(mut approvals) = self.approvals.lock() {
            if message["method"] == "serverRequest/resolved" {
                approvals.remove(&message["params"]["requestId"].to_string());
            } else if let Some(id) = message.get("id") {
                approvals.entry(id.to_string()).or_insert(false);
            }
        }
    }

    pub fn file_command(
        &self,
        script: &str,
        arguments: &[&str],
    ) -> Result<tokio::process::Command> {
        let args = &self.endpoint.args;
        if args.len() < 4 || args[0] != "exec" || args[1] != "-i" || args[3] != "codex" {
            return Err(error(
                "this session does not provide a container file transport",
            ));
        }
        let mut command = tokio::process::Command::new(&self.endpoint.program);
        command
            .args(&args[..3])
            .args(["sh", "-c", script, "--"])
            .args(arguments);
        Ok(command)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        // PTY execution replies only when the shell exits. Keep control requests
        // bounded, but do not time out an explicitly persistent terminal.
        let persistent_terminal =
            method == "command/exec" && params["tty"] == true && params["disableTimeout"] == true;
        let id = format!("remote-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| error("request lock poisoned"))?
            .insert(id.clone(), tx);
        let pending = PendingGuard {
            pending: &self.pending,
            id: id.clone(),
        };
        if self
            .outgoing
            .send(json!({"id":id,"method":method,"params":params}))
            .await
            .is_err()
        {
            return Err(error("session disconnected"));
        }
        let result = if persistent_terminal {
            Ok(rx.await)
        } else {
            tokio::time::timeout(Duration::from_secs(120), rx).await
        };
        let value = result
            .map_err(|_| error("delivery unknown; inspect session before retrying"))?
            .map_err(|_| error("session disconnected; delivery unknown"))?;
        drop(pending);
        if let Some(failure) = value.get("error") {
            return Err(error(
                failure["message"]
                    .as_str()
                    .unwrap_or("Codex request failed"),
            ));
        }
        Ok(value["result"].clone())
    }

    pub async fn reply(&self, id: Value, result: Value) -> Result<Value> {
        let key = serde_json::to_string(&id).map_err(error)?;
        {
            let mut approvals = self
                .approvals
                .lock()
                .map_err(|_| error("approval lock poisoned"))?;
            let submitted = approvals
                .get_mut(&key)
                .ok_or_else(|| error("approval is unknown or already resolved"))?;
            if *submitted {
                return Err(error("approval response was already submitted"));
            }
            *submitted = true;
        }
        self.outgoing
            .send(json!({"id":id,"result":result}))
            .await
            .map_err(error)?;
        Ok(json!({}))
    }
}

async fn attach(state: Arc<State>, endpoint: SessionEndpoint) -> Result<Arc<Backend>> {
    let connection = connect_endpoint(&endpoint).await?;
    attach_connection(state, endpoint, connection).await
}

async fn attach_connection(
    state: Arc<State>,
    endpoint: SessionEndpoint,
    connection: tokio_tungstenite::WebSocketStream<codex_start_transport::Stream>,
) -> Result<Arc<Backend>> {
    let (mut writer, mut reader) = connection.split();
    let (outgoing, mut receiver) = mpsc::channel::<Value>(64);
    let backend = Arc::new(Backend {
        info: endpoint.info.clone(),
        endpoint,
        outgoing,
        pending: Mutex::new(BTreeMap::new()),
        next: AtomicU64::new(1),
        approvals: Mutex::new(BTreeMap::new()),
        observed: Mutex::new(std::collections::BTreeSet::new()),
    });
    let writer_task = tokio::spawn(async move {
        while let Some(value) = receiver.recv().await {
            if writer
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    value.to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let current = backend.clone();
    let observation_state = state.clone();
    tokio::spawn(async move {
        while let Some(Ok(frame)) = reader.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                continue;
            };
            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if message.get("method").is_none()
                && let Some(id) = message["id"].as_str()
            {
                if let Ok(mut pending) = current.pending.lock()
                    && let Some(result) = pending.remove(id)
                {
                    let _ = result.send(message);
                }
                continue;
            }
            if message["method"] == "currentTime/read" {
                let _=current.outgoing.send(json!({"id":message["id"],"result":{"currentTimeAt":codex_start_remote::now()}})).await;
                continue;
            }
            current.track_approval(&message);
            if super::terminals::output(&state, &current.info.id.0, &message).await {
                continue;
            }
            if message["method"] == "thread/status/changed"
                && let Some(id) = message["params"]["threadId"].as_str()
            {
                if message["params"]["status"]["type"] == "notLoaded" {
                    if let Ok(mut observed) = current.observed.lock() {
                        observed.remove(id);
                    }
                } else {
                    let backend = current.clone();
                    let state = state.clone();
                    let id = id.to_owned();
                    tokio::spawn(async move {
                        observe_thread(&state, &backend, &id).await;
                    });
                }
            }
            if let Err(error) = state.publish(&current.info.id, &message) {
                tracing::warn!(%error,"remote event storage failed");
            }
        }
        writer_task.abort();
        if let Ok(mut pending) = current.pending.lock() {
            pending.clear();
        }
        let _ = state.publish(
            &current.info.id,
            &json!({"method":"session/disconnected","params":{}}),
        );
        state
            .sessions
            .lock()
            .await
            .backends
            .remove(&current.info.id.0);
    });
    backend.request("initialize",json!({"clientInfo":{"name":"codex-start-android-gateway","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
    backend
        .outgoing
        .send(json!({"method":"initialized"}))
        .await
        .map_err(error)?;
    refresh_observed(&observation_state, &backend).await;
    Ok(backend)
}

async fn observe_thread(state: &Arc<State>, backend: &Arc<Backend>, id: &str) {
    let fresh = backend
        .observed
        .lock()
        .is_ok_and(|mut set| set.insert(id.to_owned()));
    if !fresh {
        return;
    }
    // An empty live thread may not have a rollout yet. Resuming it can fail
    // with a missing-lineage error; wait until its history is persisted.
    let persisted = backend
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await
        .is_ok_and(|value| {
            value["thread"]["path"]
                .as_str()
                .is_some_and(|path| !path.is_empty())
        });
    if !persisted {
        if let Ok(mut observed) = backend.observed.lock() {
            observed.remove(id);
        }
        return;
    }
    // Rejoin only threads reported as loaded by this process. This restores
    // event delivery and pending approvals from the existing writer.
    if backend
        .request("thread/resume", json!({"threadId":id,"excludeTurns":true}))
        .await
        .is_err()
    {
        if let Ok(mut observed) = backend.observed.lock() {
            observed.remove(id);
        }
        return;
    }
    if let Ok(page) = backend
        .request(
            "thread/turns/list",
            json!({"threadId":id,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
        )
        .await
        && let Some(turn) = page["data"].as_array().and_then(|v| v.first())
        && matches!(
            turn["status"].as_str(),
            Some("completed" | "failed" | "interrupted")
        )
    {
        let _ = state.publish(
            &backend.info.id,
            &json!({"method":"turn/completed","params":{"threadId":id,"turn":turn}}),
        );
    }
}

async fn refresh_observed(state: &Arc<State>, backend: &Arc<Backend>) {
    if let Ok(loaded) = backend.request("thread/loaded/list", json!({})).await
        && let Some(ids) = loaded["data"].as_array()
    {
        for id in ids.iter().filter_map(Value::as_str) {
            observe_thread(state, backend, id).await;
        }
    }
}

pub async fn get(state: &Arc<State>, id: &str) -> Result<Arc<Backend>> {
    state
        .sessions
        .lock()
        .await
        .backends
        .get(id)
        .cloned()
        .ok_or_else(|| error("session is unavailable or still starting"))
}

/// Reuse the owner process so several clients share one serialized writer.
/// An unrelated project must never receive a thread request.
pub async fn for_thread(state: &Arc<State>, session: &str, thread: &str) -> Result<Arc<Backend>> {
    let selected = get(state, session).await?;
    let candidates: Vec<_> = state
        .sessions
        .lock()
        .await
        .backends
        .values()
        .filter(|backend| {
            backend.info.cwd == selected.info.cwd && backend.info.profile == selected.info.profile
        })
        .cloned()
        .collect();
    for backend in candidates {
        if let Ok(loaded) = backend.request("thread/loaded/list", json!({})).await
            && loaded["data"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(thread)))
        {
            return Ok(backend);
        }
    }
    Ok(selected)
}

pub async fn list(state: &Arc<State>) -> Vec<SessionInfo> {
    let mut sessions: BTreeMap<String, SessionInfo> = state
        .sessions
        .lock()
        .await
        .backends
        .values()
        .map(|b| (b.info.id.0.clone(), b.info.clone()))
        .collect();
    if let Ok(store) = SessionStore::open(state.session_directory.clone())
        && let Ok(records) = store.list()
    {
        for record in records.into_iter().take(5000) {
            sessions
                .entry(record.id.to_string())
                .or_insert_with(|| record_info(&record));
        }
    }
    sessions.into_values().collect()
}

/// Return one bounded snapshot of work that is active across connected app servers.
pub async fn active_tasks(state: &Arc<State>) -> Value {
    let backends = state
        .sessions
        .lock()
        .await
        .backends
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let checks = backends.into_iter().map(|backend| async move {
        let info = backend.info.clone();
        let result = tokio::time::timeout(Duration::from_secs(10), active_threads(backend)).await;
        (info, result)
    });
    let mut tasks = Vec::new();
    let mut unavailable = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (session, result) in futures::future::join_all(checks).await {
        let (threads, incomplete) = match result {
            Ok(result) => result,
            Err(_) => (Vec::new(), true),
        };
        if incomplete {
            unavailable.push(json!(session));
        }
        for thread in threads {
            let Some(id) = thread["id"].as_str() else {
                continue;
            };
            if seen.insert((session.id.0.clone(), id.to_owned())) {
                tasks.push(json!({"session":session,"thread":thread}));
            }
        }
    }
    if let Ok(store) = SessionStore::open(state.session_directory.clone())
        && let Ok(records) = store.list()
    {
        for record in records {
            if record.kind == SessionKind::Job
                && record.status.is_live()
                && seen.insert((record.id.to_string(), String::new()))
            {
                tasks.push(json!({"session":record_info(&record),"thread":null}));
            }
        }
    }
    json!({"data":tasks,"unavailableSessions":unavailable})
}

async fn active_threads(backend: Arc<Backend>) -> (Vec<Value>, bool) {
    let Ok(loaded) = backend
        .request("thread/loaded/list", json!({"limit":1000}))
        .await
    else {
        return (Vec::new(), true);
    };
    let Some(ids) = loaded["data"].as_array() else {
        return (Vec::new(), true);
    };
    let ids = ids
        .iter()
        .filter_map(Value::as_str)
        .take(1000)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let reads = futures::stream::iter(ids.into_iter().map(|id| {
        let backend = backend.clone();
        async move {
            backend
                .request("thread/read", json!({"threadId":id,"includeTurns":false}))
                .await
        }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    let incomplete = reads.iter().any(Result::is_err);
    let threads = reads
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter_map(|value| value.get("thread").cloned())
        .filter(|thread| thread.pointer("/status/type").and_then(Value::as_str) == Some("active"))
        .collect();
    (threads, incomplete)
}

fn record_info(record: &crate::session::SessionRecord) -> SessionInfo {
    SessionInfo {
        id: SessionId(record.id.to_string()),
        name: record.alias.clone(),
        cwd: record.cwd.as_os_str().to_string_lossy().into_owned(),
        execution_cwd: record
            .container_workdir
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
        environment: record.environment.clone(),
        profile: record.profile.clone(),
        status: serde_json::to_value(record.status)
            .ok()
            .and_then(|s| s.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into()),
        kind: if record.kind == SessionKind::Job {
            "job"
        } else {
            "persistent"
        }
        .into(),
        capabilities: if record.kind == SessionKind::Interactive && record.status.is_live() {
            vec!["codexRpc".into()]
        } else {
            Vec::new()
        },
    }
}

pub async fn control_session(state: &Arc<State>, method: &str, params: &Value) -> Result<Value> {
    let id = params["sessionId"]
        .as_str()
        .ok_or_else(|| error("missing session ID"))?;
    let uuid = uuid::Uuid::parse_str(id).map_err(error)?;
    let store = SessionStore::open(state.session_directory.clone())?;
    if method == "session/logs" {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(store.log_path(uuid))
            .await
            .map_err(error)?;
        let length = file.metadata().await.map_err(error)?.len();
        file.seek(std::io::SeekFrom::Start(length.saturating_sub(256 * 1024)))
            .await
            .map_err(error)?;
        let mut bytes = Vec::new();
        file.take(256 * 1024)
            .read_to_end(&mut bytes)
            .await
            .map_err(error)?;
        return Ok(json!({"text":String::from_utf8_lossy(&bytes),"truncated":length>256*1024}));
    }
    if let Ok(record) = store.read(uuid) {
        let restart = method == "session/restart";
        return tokio::task::spawn_blocking(move || {
            let result = if restart {
                crate::session::restart_record(&store, &record)
            } else {
                crate::session::stop_record(&store, &record)
            }?;
            serde_json::to_value(record_info(&result)).map_err(error)
        })
        .await
        .map_err(error)?;
    }
    if method == "session/restart" {
        return Err(error("restart this adapter from its local client"));
    }
    let backend = get(state, id).await?;
    if backend.endpoint.args.len() < 3 || backend.endpoint.args[0] != "exec" {
        return Err(error("this session cannot be stopped by the daemon"));
    }
    let status = tokio::process::Command::new(&backend.endpoint.program)
        .args(["stop", &backend.endpoint.args[2]])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .await
        .map_err(error)?;
    if !status.success() {
        return Err(error("container stop failed"));
    }
    let _ = tokio::fs::remove_file(super::registry::directory()?.join(format!("{id}.json"))).await;
    Ok(json!({"stopped":true}))
}

pub async fn monitor(state: Arc<State>) {
    loop {
        if let Ok(endpoints) = discover(&state) {
            for endpoint in endpoints {
                if state
                    .sessions
                    .lock()
                    .await
                    .backends
                    .contains_key(&endpoint.info.id.0)
                {
                    continue;
                }
                if let Ok(backend) = attach(state.clone(), endpoint).await {
                    state
                        .sessions
                        .lock()
                        .await
                        .backends
                        .insert(backend.info.id.0.clone(), backend);
                }
            }
        }
        let active: Vec<_> = state
            .sessions
            .lock()
            .await
            .backends
            .values()
            .cloned()
            .collect();
        for backend in active {
            refresh_observed(&state, &backend).await;
        }
        observe_jobs(&state);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn observe_jobs(state: &Arc<State>) {
    let Ok(store) = SessionStore::open(state.session_directory.clone()) else {
        return;
    };
    let Ok(records) = store.list() else {
        return;
    };
    for record in records {
        if record.kind == SessionKind::Job
            && let Some(exit_code) = record.exit_code
        {
            let _ = state.publish(
                &SessionId(record.id.to_string()),
                &json!({"method":"job/exited","params":{"exitCode":exit_code,"name":record.alias}}),
            );
        }
    }
}

fn discover(state: &State) -> Result<Vec<SessionEndpoint>> {
    let store = SessionStore::open(state.session_directory.clone())?;
    let mut endpoints = Vec::new();
    for record in store.list()? {
        if record.kind != SessionKind::Interactive || !record.status.is_live() {
            continue;
        }
        endpoints.push(SessionEndpoint {
            info: SessionInfo {
                id: SessionId(record.id.to_string()),
                name: record.alias,
                cwd: record.cwd.as_os_str().to_string_lossy().into_owned(),
                execution_cwd: record
                    .container_workdir
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned(),
                environment: record.environment,
                profile: record.profile,
                status: "running".into(),
                kind: "persistent".into(),
                capabilities: vec!["codexRpc".into()],
            },
            program: record
                .runtime_program
                .as_os_str()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "exec".into(),
                "-i".into(),
                record.container_name,
                "codex".into(),
                "app-server".into(),
                "proxy".into(),
                "--sock".into(),
                APP_SERVER_SOCKET.into(),
            ],
            owner_pid: record.supervisor_pid,
        });
    }
    for entry in std::fs::read_dir(super::registry::directory()?).map_err(error)? {
        let entry = entry.map_err(error)?;
        if !entry.file_type().map_err(error)?.is_file()
            || entry.path().extension().is_none_or(|x| x != "json")
        {
            continue;
        }
        let bytes = std::fs::read(entry.path()).map_err(error)?;
        if bytes.len() > 64 * 1024 {
            continue;
        }
        if let Ok(endpoint) = serde_json::from_slice::<SessionEndpoint>(&bytes) {
            endpoints.push(endpoint);
        }
    }
    Ok(endpoints)
}

pub async fn create(state: &Arc<State>, params: &Value) -> Result<Value> {
    let cwd = params["cwd"]
        .as_str()
        .ok_or_else(|| error("new session requires a host directory"))?;
    let cwd = std::fs::canonicalize(cwd).map_err(error)?;
    if !cwd.is_dir() {
        return Err(error("project must be a directory"));
    }
    let mut command = tokio::process::Command::new(std::env::current_exe().map_err(error)?);
    if let Some(config) = &state.config {
        command.arg("--config").arg(config);
    }
    command.args([
        "--output",
        "json",
        "session",
        "start",
        "--persistent",
        "--no-tty",
    ]);
    if let Some(environment) = params["environment"].as_str() {
        command.arg(environment);
    }
    if params["worktree"].as_bool() == Some(true) {
        command.arg("--worktree");
    } else if params["worktree"].as_bool() == Some(false) {
        command.arg("--no-worktree");
    }
    for (key, flag) in [
        ("name", "--name"),
        ("home", "--home"),
        ("profile", "--profile"),
    ] {
        if let Some(value) = params[key].as_str() {
            command.arg(flag).arg(value);
        }
    }
    command.current_dir(cwd).stdin(Stdio::null());
    let output = command.output().await.map_err(error)?;
    if !output.status.success() {
        return Err(error(String::from_utf8_lossy(&output.stderr)));
    }
    let text = String::from_utf8(output.stdout).map_err(error)?;
    let record = text
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .ok_or_else(|| error("launcher did not return session metadata"))?;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| error("launcher did not return a session ID"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    while tokio::time::Instant::now() < deadline {
        if let Ok(backend) = get(state, id).await {
            return serde_json::to_value(&backend.info).map_err(error);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(error(format!(
        "session {id} was created, but is not ready; inspect its host logs before retrying"
    )))
}

#[cfg(test)]
pub(super) async fn connect_fixture(state: &Arc<State>, endpoint: SessionEndpoint) -> Arc<Backend> {
    let backend = attach(state.clone(), endpoint).await.unwrap();
    state
        .sessions
        .lock()
        .await
        .backends
        .insert(backend.info.id.0.clone(), backend.clone());
    backend
}

#[cfg(test)]
pub(super) fn register_fixture(state: &Arc<State>, endpoint: SessionEndpoint) {
    let (outgoing, _) = mpsc::channel(1);
    let backend = Arc::new(Backend {
        info: endpoint.info.clone(),
        endpoint,
        outgoing,
        pending: Mutex::new(BTreeMap::new()),
        next: AtomicU64::new(1),
        approvals: Mutex::new(BTreeMap::new()),
        observed: Mutex::new(std::collections::BTreeSet::new()),
    });
    state
        .sessions
        .try_lock()
        .unwrap()
        .backends
        .insert(backend.info.id.0.clone(), backend);
}
