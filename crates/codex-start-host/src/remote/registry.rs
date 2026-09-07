//! Private registration and socket bridging for GUI-owned app-server processes.

use super::error;
use crate::{
    cli::Cli,
    error::Result,
    paths::{AppPaths, atomic_write, create_private_dir},
};
use codex_start_remote::{SessionEndpoint, SessionId, SessionInfo};
use futures::{SinkExt, StreamExt};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

pub const WORKER_ID: &str = "CODEX_START_REMOTE_ADAPTER_ID";
pub fn directory() -> Result<PathBuf> {
    let root = AppPaths::discover()?.data.join("remote-sessions");
    create_private_dir(&root)?;
    Ok(root)
}
pub fn socket(id: &str) -> String {
    format!("/tmp/codex-start-{id}/app-server.sock")
}

pub fn register(
    id: &str,
    runtime: &crate::runtime::Runtime,
    container: &str,
    cwd: &Path,
    execution_cwd: &Path,
    environment: &str,
    profile: Option<&str>,
) -> Result<()> {
    let endpoint = SessionEndpoint {
        info: SessionInfo {
            id: SessionId(id.into()),
            name: cwd
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            execution_cwd: execution_cwd.to_string_lossy().into_owned(),
            environment: environment.into(),
            profile: profile.map(str::to_owned),
            status: "running".into(),
            kind: "adapter".into(),
            capabilities: vec!["codexRpc".into()],
        },
        program: runtime.program().to_string_lossy().into_owned(),
        args: vec![
            "exec".into(),
            "-i".into(),
            container.into(),
            "codex".into(),
            "app-server".into(),
            "proxy".into(),
            "--sock".into(),
            socket(id),
        ],
        owner_pid: Some(std::process::id()),
    };
    atomic_write(
        &directory()?.join(format!("{id}.json")),
        &serde_json::to_string(&endpoint).map_err(error)?,
    )
}

pub async fn enabled(cli: &Cli) -> bool {
    let config = cli
        .config
        .as_deref()
        .and_then(|p| std::fs::canonicalize(p).ok());
    enabled_config(config.as_deref()).await
}
async fn enabled_config(config: Option<&Path>) -> bool {
    match super::state::root(config) {
        Ok(root) => super::lifecycle::control(&root, serde_json::json!({"method":"status"}))
            .await
            .is_ok(),
        Err(_) => false,
    }
}
pub async fn enabled_for(config: &Path) -> bool {
    enabled_config(None).await
        || if let Ok(config) = std::fs::canonicalize(config) {
            enabled_config(Some(&config)).await
        } else {
            false
        }
}

pub async fn bridge(cli: &Cli) -> Result<u8> {
    let id = uuid::Uuid::new_v4().to_string();
    let executable = std::env::current_exe().map_err(error)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(std::env::args_os().skip(1))
        .env(WORKER_ID, &id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command.process_group(0);
    let mut worker = command.spawn().map_err(error)?;
    let path = directory()?.join(format!("{id}.json"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut connection = loop {
        if let Some(status) = worker.try_wait().map_err(error)? {
            return Err(error(format!("adapter server exited: {status}")));
        }
        if tokio::time::Instant::now() > deadline {
            return Err(error("adapter socket startup timed out"));
        }
        if let Ok(bytes) = tokio::fs::read(&path).await
            && let Ok(endpoint) = serde_json::from_slice::<SessionEndpoint>(&bytes)
            && let Ok(connection) = super::sessions::connect_endpoint(&endpoint).await
        {
            break connection;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
    // A native thread allows terminal EOF without an uncancellable Tokio stdin task.
    std::thread::spawn(move || {
        use std::io::{BufRead, Read};
        let input = std::io::stdin();
        let mut input = input.lock();
        loop {
            let mut bytes = Vec::new();
            if input
                .by_ref()
                .take(8 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes)
                .unwrap_or(0)
                == 0
            {
                break;
            }
            if bytes.len() > 8 * 1024 * 1024 {
                break;
            }
            if sender.blocking_send(bytes).is_err() {
                break;
            }
        }
    });
    let mut output = tokio::io::stdout();
    loop {
        tokio::select! {
            line=receiver.recv()=>match line{Some(line)=>{let text=String::from_utf8(line).map_err(error)?;connection.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await.map_err(error)?;},None=>break},
            frame=connection.next()=>match frame{Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))=>{output.write_all(text.as_bytes()).await.map_err(error)?;output.write_all(b"\n").await.map_err(error)?;output.flush().await.map_err(error)?;},Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))|None=>break,Some(Err(e))=>return Err(error(e)),_=>{}}
        }
    }
    let _ = connection.close(None).await;
    // The explicitly started gateway retains the registered server for mobile clients.
    let _ = cli;
    Ok(0)
}
