//! Device-owned PTYs. A screen or transport disconnect does not close a terminal.
use super::{error, sessions::Backend, state::State};
use crate::error::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_start_remote::DeviceId;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

const BUFFER: usize = 2 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Default)]
pub struct Terminals {
    entries: BTreeMap<String, Arc<Terminal>>,
    next_order: u64,
    // Keep capturing output after close until command/exec has fully returned.
    captured: BTreeSet<(String, String)>,
}
impl Drop for Terminals {
    fn drop(&mut self) {
        for terminal in self.entries.values() {
            if let Ok(mut host) = terminal.host.lock() {
                host.take();
            }
        }
    }
}

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    end: u64,
    exit: Option<u32>,
    drained: bool,
    failure: String,
}
impl Output {
    fn done(&self) -> bool {
        self.exit.is_some() && self.drained
    }
}
struct Input {
    offset: u64,
    last: Option<(u64, String)>,
}
struct HostPty {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
}
impl Drop for HostPty {
    fn drop(&mut self) {
        // Stop the foreground group before dropping the writer, which can send EOF.
        if self.child.try_wait().ok().flatten().is_none() {
            if let Some(group) = self.master.process_group_leader() {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(group),
                    nix::sys::signal::Signal::SIGHUP,
                );
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
struct Terminal {
    id: String,
    order: u64,
    device: DeviceId,
    session: String,
    target: String,
    cwd: String,
    name: Mutex<String>,
    output: Mutex<Output>,
    changed: Notify,
    input: tokio::sync::Mutex<Input>,
    host: Mutex<Option<HostPty>>,
    backend: Option<Arc<Backend>>,
}
impl Terminal {
    fn append(&self, bytes: &[u8]) {
        if let Ok(mut output) = self.output.lock() {
            output.end += bytes.len() as u64;
            output.bytes.extend(bytes);
            let excess = output.bytes.len().saturating_sub(BUFFER);
            output.bytes.drain(..excess);
        }
        self.changed.notify_waiters();
    }
    fn finish(&self, exit: u32, failure: String) {
        if let Ok(mut output) = self.output.lock() {
            output.exit = Some(exit);
            output.failure = failure;
            if self.backend.is_some() {
                output.drained = true;
            }
        }
        self.changed.notify_waiters();
    }
    fn info(&self) -> Result<Value> {
        let output = self.output.lock().map_err(error)?;
        Ok(
            json!({"id":self.id,"order":self.order,"name":*self.name.lock().map_err(error)?,"target":self.target,
            "sessionId":self.session,"cwd":self.cwd,"status":if output.done(){"exited"}else{"running"},
            "exitCode":output.exit,"error":output.failure}),
        )
    }
    async fn close(&self) -> Result<()> {
        if let Some(backend) = &self.backend {
            if self.output.lock().map_err(error)?.exit.is_none() {
                backend
                    .request("command/exec/terminate", json!({"processId":self.id}))
                    .await?;
            }
        } else {
            let host = self.host.lock().map_err(error)?.take();
            tokio::task::spawn_blocking(move || drop(host))
                .await
                .map_err(error)?;
        }
        self.finish(1, String::new());
        Ok(())
    }
}

fn field<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params[name]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
        .ok_or_else(|| error(format!("missing or invalid {name}")))
}
fn size(params: &Value) -> Result<PtySize> {
    let dimension = |name: &str, default: u64| -> Result<u16> {
        let value = params[name].as_u64().unwrap_or(default);
        if !(1..=1000).contains(&value) {
            return Err(error("terminal size must be between 1 and 1000"));
        }
        u16::try_from(value).map_err(error)
    };
    Ok(PtySize {
        cols: dimension("cols", 80)?,
        rows: dimension("rows", 24)?,
        pixel_width: 0,
        pixel_height: 0,
    })
}

pub async fn dispatch(
    state: &Arc<State>,
    device: &DeviceId,
    method: &str,
    params: &Value,
) -> Result<Value> {
    if method == "terminal/open" {
        return open(state, device, params).await;
    }
    if method == "terminal/list" {
        let terminals = state.terminals.lock().await;
        let mut entries: Vec<_> = terminals
            .entries
            .values()
            .filter(|t| &t.device == device)
            .collect();
        entries.sort_by_key(|t| t.order);
        let data = entries
            .iter()
            .map(|t| t.info())
            .collect::<Result<Vec<_>>>()?;
        return Ok(json!({"data":data}));
    }
    let id = field(params, "terminalId")?;
    let terminal = state
        .terminals
        .lock()
        .await
        .entries
        .get(id)
        .filter(|t| &t.device == device)
        .cloned()
        .ok_or_else(|| error("terminal is closed or belongs to another device"))?;
    match method {
        "terminal/read" => read(&terminal, params).await,
        "terminal/write" => write(&terminal, params).await,
        "terminal/status" => {
            let mut info = terminal.info()?;
            info["inputOffset"] = json!(terminal.input.lock().await.offset);
            Ok(info)
        }
        "terminal/resize" => {
            let dimensions = size(params)?;
            if let Some(backend) = &terminal.backend {
                backend.request("command/exec/resize", json!({"processId":id,"size":{"cols":dimensions.cols,"rows":dimensions.rows}})).await?;
            } else if let Some(host) = terminal.host.lock().map_err(error)?.as_ref() {
                host.master.resize(dimensions).map_err(error)?;
            }
            Ok(json!({}))
        }
        "terminal/rename" => {
            let name = field(params, "name")?.trim();
            if name.is_empty() || name.chars().count() > 80 {
                return Err(error("terminal name must contain 1 to 80 characters"));
            }
            name.clone_into(&mut *terminal.name.lock().map_err(error)?);
            terminal.info()
        }
        "terminal/close" => {
            terminal.close().await?;
            state.terminals.lock().await.entries.remove(id);
            Ok(json!({}))
        }
        _ => Err(error("unknown terminal operation")),
    }
}

async fn read(terminal: &Terminal, params: &Value) -> Result<Value> {
    let offset = params["offset"].as_u64().unwrap_or(0);
    let notified = terminal.changed.notified();
    let wait = {
        let output = terminal.output.lock().map_err(error)?;
        offset == output.end && !output.done()
    };
    if wait {
        let _ = tokio::time::timeout(Duration::from_millis(750), notified).await;
    }
    let output = terminal.output.lock().map_err(error)?;
    if offset > output.end {
        return Err(error("terminal offset is ahead of output"));
    }
    let start = output.end - output.bytes.len() as u64;
    let actual = offset.max(start);
    let bytes: Vec<u8> = output
        .bytes
        .iter()
        .skip(usize::try_from(actual - start).map_err(error)?)
        .take(CHUNK)
        .copied()
        .collect();
    Ok(
        json!({"offset":actual,"nextOffset":actual + bytes.len() as u64,"endOffset":output.end,"dataBase64":STANDARD.encode(bytes),
                "truncated":offset < start,"status":if output.done(){"exited"}else{"running"},"exitCode":output.exit,"error":output.failure}),
    )
}

async fn write(terminal: &Arc<Terminal>, params: &Value) -> Result<Value> {
    let id = &terminal.id;
    let encoded = field(params, "dataBase64")?;
    let bytes = STANDARD.decode(encoded).map_err(error)?;
    if bytes.len() > 2048 {
        return Err(error("terminal input chunk exceeds 2048 bytes"));
    }
    let length = bytes.len();
    let offset = params["offset"]
        .as_u64()
        .ok_or_else(|| error("input offset is required"))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let mut input = terminal.input.lock().await;
    if input.last.as_ref() == Some(&(offset, hash.clone())) {
        return Ok(json!({"offset":input.offset}));
    }
    if offset != input.offset {
        return Err(error(
            "terminal input offset changed; reconnect before typing",
        ));
    }
    if terminal.output.lock().map_err(error)?.exit.is_some() {
        return Err(error("terminal has exited"));
    }
    if let Some(backend) = &terminal.backend {
        backend
            .request(
                "command/exec/write",
                json!({"processId":id,"deltaBase64":encoded}),
            )
            .await?;
    } else {
        let current = terminal.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let writer = current
                .host
                .lock()
                .map_err(error)?
                .as_ref()
                .ok_or_else(|| error("terminal is closed"))?
                .writer
                .clone();
            writer
                .lock()
                .map_err(error)?
                .write_all(&bytes)
                .map_err(error)
        })
        .await
        .map_err(error)??;
    }
    input.offset += length as u64;
    input.last = Some((offset, hash));
    Ok(json!({"offset":input.offset}))
}

async fn open(state: &Arc<State>, device: &DeviceId, params: &Value) -> Result<Value> {
    let id = field(params, "terminalId")?;
    uuid::Uuid::parse_str(id).map_err(error)?;
    let target = field(params, "target")?;
    if !matches!(target, "host" | "session") {
        return Err(error("choose host or session"));
    }
    let backend = if target == "session" {
        Some(super::sessions::get(state, field(params, "sessionId")?).await?)
    } else {
        None
    };
    let mut terminals = state.terminals.lock().await;
    if let Some(existing) = terminals.entries.get(id) {
        if &existing.device != device {
            return Err(error("terminal ID is already in use"));
        }
        return existing.info();
    }
    if terminals.captured.iter().any(|(_, process)| process == id) {
        return Err(error("terminal ID is still closing"));
    }
    if terminals.entries.len() >= 32
        || terminals
            .entries
            .values()
            .filter(|t| &t.device == device)
            .count()
            >= 8
    {
        return Err(error(
            "close a terminal before opening another (8 per device)",
        ));
    }
    let dimensions = size(params)?;
    let cwd = directory(backend.as_deref(), params)?;
    let command = params["command"].as_str().unwrap_or_default();
    if command.len() > 8192 || command.contains('\0') {
        return Err(error("invalid terminal command"));
    }
    let label = if target == "host" { "Host" } else { "Session" };
    let name = (1..=33)
        .map(|n| format!("{label} {n}"))
        .find(|candidate| {
            !terminals
                .entries
                .values()
                .any(|t| t.name.lock().is_ok_and(|name| *name == *candidate))
        })
        .ok_or_else(|| error("no terminal names available"))?;
    terminals.next_order += 1;
    let terminal = Arc::new(Terminal {
        id: id.into(),
        order: terminals.next_order,
        device: device.clone(),
        session: backend
            .as_ref()
            .map(|b| b.info.id.0.clone())
            .unwrap_or_default(),
        target: target.into(),
        cwd: cwd.clone(),
        name: Mutex::new(name),
        output: Mutex::new(Output::default()),
        changed: Notify::new(),
        input: tokio::sync::Mutex::new(Input {
            offset: 0,
            last: None,
        }),
        host: Mutex::new(None),
        backend: backend.clone(),
    });
    if let Some(backend) = backend {
        terminals.entries.insert(id.into(), terminal.clone());
        terminals
            .captured
            .insert((terminal.session.clone(), terminal.id.clone()));
        launch_session(state, &terminal, backend, command, dimensions);
    } else {
        launch_host(&terminal, command, dimensions).await?;
        terminals.entries.insert(id.into(), terminal.clone());
    }
    terminal.info()
}

fn directory(backend: Option<&Backend>, params: &Value) -> Result<String> {
    if let Some(backend) = backend {
        return Ok(backend.info.execution_cwd.clone());
    }
    let path = params["cwd"]
        .as_str()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .ok_or_else(|| error("host directory is required"))?;
    let path = std::fs::canonicalize(path).map_err(error)?;
    if !path.is_dir() {
        return Err(error("host working directory is not a directory"));
    }
    Ok(path.to_string_lossy().into_owned())
}

fn launch_session(
    state: &Arc<State>,
    terminal: &Arc<Terminal>,
    backend: Arc<Backend>,
    command: &str,
    dimensions: PtySize,
) {
    let state = Arc::downgrade(state);
    let current = terminal.clone();
    let argv = if command.is_empty() {
        vec![
            "sh".to_owned(),
            "-lc".into(),
            "exec \"${SHELL:-/bin/sh}\" -l".into(),
        ]
    } else {
        vec!["sh".into(), "-lc".into(), command.into()]
    };
    tokio::spawn(async move {
        let result = backend.request("command/exec", json!({"command":argv,"cwd":current.cwd,"processId":current.id,
                "tty":true,"streamStdin":true,"streamStdoutStderr":true,"disableOutputCap":true,"disableTimeout":true,
                "sandboxPolicy":{"type":"dangerFullAccess"},"env":{"TERM":"xterm-256color","COLORTERM":"truecolor"},
                "size":{"cols":dimensions.cols,"rows":dimensions.rows}})).await;
        match result {
            Ok(value) => current.finish(
                value["exitCode"]
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(1),
                String::new(),
            ),
            Err(failure) => current.finish(1, failure.to_string()),
        }
        if let Some(state) = state.upgrade() {
            state
                .terminals
                .lock()
                .await
                .captured
                .remove(&(current.session.clone(), current.id.clone()));
        }
    });
}

async fn launch_host(terminal: &Arc<Terminal>, command: &str, dimensions: PtySize) -> Result<()> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/') && std::path::Path::new(s).is_file())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut builder = CommandBuilder::new(shell);
    if command.is_empty() {
        builder.arg("-l");
    } else {
        builder.args(["-lc", command]);
    }
    builder.cwd(&terminal.cwd);
    builder.env("TERM", "xterm-256color");
    builder.env("COLORTERM", "truecolor");
    let (host, mut reader) = tokio::task::spawn_blocking(move || -> Result<_> {
        let pair = portable_pty::native_pty_system()
            .openpty(dimensions)
            .map_err(error)?;
        let reader = pair.master.try_clone_reader().map_err(error)?;
        let writer = pair.master.take_writer().map_err(error)?;
        let child = pair.slave.spawn_command(builder).map_err(error)?;
        Ok((
            HostPty {
                master: pair.master,
                writer: Arc::new(Mutex::new(writer)),
                child,
            },
            reader,
        ))
    })
    .await
    .map_err(error)??;
    *terminal.host.lock().map_err(error)? = Some(host);
    let current = terminal.clone();
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0; 8192];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => current.append(&bytes[..n]),
            }
        }
        if let Ok(mut output) = current.output.lock() {
            output.drained = true;
        }
        current.changed.notify_waiters();
    });
    let current = Arc::downgrade(terminal);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let Some(current) = current.upgrade() else {
                break;
            };
            let exited = current
                .host
                .lock()
                .ok()
                .and_then(|mut h| h.as_mut().and_then(|h| h.child.try_wait().ok().flatten()));
            if let Some(exit) = exited {
                current.finish(exit.exit_code(), String::new());
                break;
            }
            if current.host.lock().map(|h| h.is_none()).unwrap_or(true) {
                break;
            }
        }
    });
    Ok(())
}

/// Consume terminal output before it can enter the persistent event journal.
pub async fn output(state: &Arc<State>, session: &str, message: &Value) -> bool {
    if message["method"] != "command/exec/outputDelta" {
        return false;
    }
    let Some(id) = message["params"]["processId"].as_str() else {
        return false;
    };
    let terminals = state.terminals.lock().await;
    let captured = terminals.captured.contains(&(session.into(), id.into()));
    let terminal = terminals
        .entries
        .get(id)
        .filter(|t| t.session == session)
        .cloned();
    drop(terminals);
    if let Some(terminal) = terminal {
        if let Some(data) = message["params"]["deltaBase64"]
            .as_str()
            .and_then(|s| STANDARD.decode(s).ok())
        {
            terminal.append(&data);
        }
        return true;
    }
    captured
}

/// Revocation closes device shells even when the device has no connection.
pub async fn reap(state: &Arc<State>, all: bool) {
    let entries: Vec<_> = state
        .terminals
        .lock()
        .await
        .entries
        .values()
        .cloned()
        .collect();
    futures::future::join_all(entries.iter().map(|terminal| async {
        if (all || !state.db(|s| s.active(&terminal.device)).unwrap_or(false))
            && tokio::time::timeout(Duration::from_secs(5), terminal.close())
                .await
                .is_ok_and(|r| r.is_ok())
        {
            state.terminals.lock().await.entries.remove(&terminal.id);
        }
    }))
    .await;
}

pub async fn monitor(state: Arc<State>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        reap(&state, false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<State>, DeviceId) {
        let root = tempfile::tempdir().unwrap();
        let state = State::open(
            root.path().into(),
            Some(root.path().join("config.toml")),
            super::super::DaemonOptions {
                bind: "127.0.0.1:0".parse().unwrap(),
                no_yggdrasil: true,
                advertise_host: None,
            },
            String::new(),
        )
        .unwrap();
        (root, state, DeviceId::generate())
    }

    async fn start(state: &Arc<State>, device: &DeviceId, command: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        dispatch(state, device, "terminal/open", &json!({"terminalId":id,"target":"host","cwd":state.root,"command":command,"cols":93,"rows":31})).await.unwrap();
        id
    }

    async fn text_until(state: &Arc<State>, device: &DeviceId, id: &str, text: &str) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut result = String::new();
            let mut offset = 0;
            loop {
                let part = dispatch(
                    state,
                    device,
                    "terminal/read",
                    &json!({"terminalId":id,"offset":offset}),
                )
                .await
                .unwrap();
                offset = part["nextOffset"].as_u64().unwrap();
                result.push_str(&String::from_utf8_lossy(
                    &STANDARD
                        .decode(part["dataBase64"].as_str().unwrap())
                        .unwrap(),
                ));
                if result.contains(text) {
                    return result;
                }
                assert_ne!(part["status"], "exited", "{result}");
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn host_pty_is_interactive_private_resizable_and_replayable() {
        let (_root, state, device) = fixture();
        let id = start(&state, &device, "stty size; exec /bin/sh").await;
        text_until(&state, &device, &id, "31 93").await;
        let input = json!({"terminalId":id,"offset":0,"dataBase64":STANDARD.encode("printf '\\nINPUT_OK\\n'\r")});
        let first = dispatch(&state, &device, "terminal/write", &input)
            .await
            .unwrap();
        assert_eq!(
            first,
            dispatch(&state, &device, "terminal/write", &input)
                .await
                .unwrap()
        );
        text_until(&state, &device, &id, "\r\nINPUT_OK\r\n").await;
        assert!(
            dispatch(
                &state,
                &DeviceId::generate(),
                "terminal/read",
                &json!({"terminalId":id})
            )
            .await
            .is_err()
        );
        dispatch(
            &state,
            &device,
            "terminal/resize",
            &json!({"terminalId":id,"cols":77,"rows":19}),
        )
        .await
        .unwrap();
        dispatch(&state, &device, "terminal/write", &json!({"terminalId":id,"offset":first["offset"],"dataBase64":STANDARD.encode("stty size\r")})).await.unwrap();
        text_until(&state, &device, &id, "19 77").await;
        dispatch(
            &state,
            &device,
            "terminal/rename",
            &json!({"terminalId":id,"name":"Build"}),
        )
        .await
        .unwrap();
        let info = dispatch(&state, &device, "terminal/list", &json!({}))
            .await
            .unwrap();
        assert_eq!(info["data"][0]["name"], "Build");
        dispatch(&state, &device, "terminal/close", &json!({"terminalId":id}))
            .await
            .unwrap();
        assert!(state.terminals.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn final_output_is_drained_and_revocation_closes_all_windows() {
        let (_root, state, device) = fixture();
        let id = start(&state, &device, "printf FINAL; exit 7").await;
        let terminal = state.terminals.lock().await.entries[&id].clone();
        tokio::time::timeout(Duration::from_secs(10), async {
            while terminal.info().unwrap()["status"] != "exited" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(terminal.info().unwrap()["exitCode"], 7);
        text_until(&state, &device, &id, "FINAL").await;
        start(&state, &device, "exec /bin/sh").await;
        reap(&state, false).await; // An unregistered device is not active.
        assert!(state.terminals.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn output_is_bounded_and_loss_is_explicit() {
        let (_root, state, device) = fixture();
        let id = start(&state, &device, "exec /bin/sh").await;
        let terminal = state.terminals.lock().await.entries[&id].clone();
        terminal.append(&vec![b'x'; BUFFER + 100]);
        let part = dispatch(
            &state,
            &device,
            "terminal/read",
            &json!({"terminalId":id,"offset":0}),
        )
        .await
        .unwrap();
        assert_eq!(part["truncated"], true);
        assert!(part["offset"].as_u64().unwrap() >= 100);
        assert_eq!(
            STANDARD
                .decode(part["dataBase64"].as_str().unwrap())
                .unwrap()
                .len(),
            CHUNK
        );
        reap(&state, true).await;
    }
}
