//! TLS gateway, authenticated event streams, and private local control.

use super::{DaemonOptions, error, state::State};
use crate::{error::Result, paths::atomic_write};
use axum::{
    Json, Router,
    extract::{
        State as AxumState, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use codex_start_remote::{ClientMessage, DeviceId, Registration, RequestId, ServerMessage, crypto};
use futures::{SinkExt, StreamExt};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

pub async fn run(root: PathBuf, config: Option<PathBuf>, mut options: DaemonOptions) -> Result<u8> {
    let _lock = crate::locking::RunLock::acquire(&root, "daemon")?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (tls, fingerprint) = codex_start_transport::tls::identity(&root).map_err(error)?;
    let direct = tokio::net::TcpListener::bind(options.bind)
        .await
        .map_err(error)?;
    options.bind = direct.local_addr().map_err(error)?;
    atomic_write(
        &root.join("options.json"),
        &serde_json::to_string(&options).map_err(error)?,
    )?;
    let control_path = root.join("control.sock");
    let control = bind_control(&control_path)?;
    let state = State::open(root, config, options, fingerprint)?;
    let router = Router::new()
        .route("/v1/discovery", get(discovery))
        .route("/v1/ws", get(upgrade))
        .with_state(state.clone());
    let monitor = tokio::spawn(super::sessions::monitor(state.clone()));
    // Build the read-only index while the phone connects. Subsequent pages reuse it.
    let history_state = state.clone();
    tokio::spawn(async move {
        if let Err(failure) = super::history::list(&history_state, json!({"limit":1})).await {
            tracing::warn!(%failure, "local history preload failed");
        }
    });
    let terminal_monitor = tokio::spawn(super::terminals::monitor(state.clone()));
    let admin = tokio::spawn(admin_loop(control, state.clone()));
    let overlay = if state.options.no_yggdrasil {
        *state
            .overlay_status
            .lock()
            .map_err(|_| error("status lock poisoned"))? = "disabled".into();
        None
    } else {
        Some(tokio::spawn(overlay_loop(
            state.clone(),
            router.clone(),
            tls.clone(),
        )))
    };
    let permits = Arc::new(tokio::sync::Semaphore::new(64));
    loop {
        tokio::select! {
            accepted=direct.accept()=>{let(stream,_)=accepted.map_err(error)?;if let Ok(permit)=permits.clone().try_acquire_owned(){let router=router.clone();let tls=tls.clone();tokio::spawn(async move{let _permit=permit;serve(Box::new(stream),router,tls).await;});}},
            ()=state.stop.notified()=>break,
            ()=crate::app::termination_signal()=>break
        }
    }
    terminal_monitor.abort();
    super::terminals::reap(&state, true).await;
    monitor.abort();
    admin.abort();
    if let Some(task) = overlay {
        task.abort();
    }
    let _ = std::fs::remove_file(control_path);
    Ok(0)
}

fn bind_control(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(error)?;
        }
        Ok(_) => return Err(error("control endpoint must be a Unix socket")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(error(e)),
    }
    let listener = tokio::net::UnixListener::bind(path).map_err(error)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(error)?;
    Ok(listener)
}

async fn serve(
    stream: codex_start_transport::Stream,
    router: Router,
    tls: tokio_rustls::TlsAcceptor,
) {
    let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(15), tls.accept(stream)).await
    else {
        return;
    };
    let service = TowerToHyperService::new(router);
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

async fn overlay_loop(state: Arc<State>, router: Router, tls: tokio_rustls::TlsAcceptor) {
    loop {
        let Ok(key) = state.db(|s| s.secret("yggdrasil_key")) else {
            return;
        };
        let Ok(group) = state.db(|s| s.secret("group_password")) else {
            return;
        };
        match codex_start_transport::overlay::Overlay::start(
            &key,
            &group,
            &state.root.join("peers.json"),
            Some(state.options.bind.port()),
        )
        .await
        {
            Ok(overlay) => {
                let overlay = Arc::new(overlay);
                if let Ok(mut current) = state.overlay.lock() {
                    *current = Some(Arc::downgrade(&overlay));
                }
                if let Ok(mut status) = state.overlay_status.lock() {
                    *status = format!("connected: {}", overlay.address());
                }
                let permits = Arc::new(tokio::sync::Semaphore::new(32));
                let mut health = tokio::time::interval(Duration::from_secs(5));
                loop {
                    let stream = tokio::select! {
                        stream = overlay.accept() => if let Some(stream) = stream { stream } else { break; },
                        _ = health.tick() => {
                            let connected = overlay.is_connected().await;
                            if let Ok(mut status) = state.overlay_status.lock() { *status = format!("{}: {}", if connected { "connected" } else { "waiting for peers" }, overlay.address()); }
                            continue;
                        }
                    };
                    if let Ok(permit) = permits.clone().try_acquire_owned() {
                        let router = router.clone();
                        let tls = tls.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            serve(stream, router, tls).await;
                        });
                    }
                }
            }
            Err(error) => {
                if let Ok(mut status) = state.overlay_status.lock() {
                    *status = format!("unavailable: {error}");
                }
                tracing::warn!(%error,"Yggdrasil connection will retry");
            }
        }
        if let Ok(mut current) = state.overlay.lock() {
            *current = None;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn discovery(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    Json(state.discovery.clone()).into_response()
}
async fn upgrade(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(codex_start_remote::MAX_MESSAGE_BYTES)
        .max_frame_size(codex_start_remote::MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            connection(socket, state).await;
        })
        .into_response()
}

async fn send(sender: &mpsc::Sender<ServerMessage>, message: ServerMessage) -> bool {
    sender.send(message).await.is_ok()
}

async fn connection(socket: WebSocket, state: Arc<State>) {
    let (mut sink, mut stream) = socket.split();
    let (sender, mut outgoing) = mpsc::channel::<ServerMessage>(8);
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                break;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let nonce = crypto::random_secret();
    let fingerprint = state.discovery.fingerprint.clone();
    send(
        &sender,
        ServerMessage::Challenge {
            nonce: nonce.clone(),
            fingerprint: fingerprint.clone(),
            discovery: state.discovery.clone(),
        },
    )
    .await;
    let mut device: Option<DeviceId> = None;
    let mut subscription: Option<tokio::task::JoinHandle<()>> = None;
    let mut authentication_checks = tokio::time::interval(Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    let mut registration_attempts = 0_u32;
    let request_slots = Arc::new(tokio::sync::Semaphore::new(16));
    loop {
        tokio::select! {
            _=authentication_checks.tick()=>{
                if device.as_ref().is_some_and(|id|!state.db(|s|s.active(id)).unwrap_or(false)) || (device.is_none() && started.elapsed()>Duration::from_secs(600)){break;}
            },
            frame=stream.next()=>{
                let Some(Ok(Message::Text(text)))=frame else{break;};
                let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else { break; };
                match message {
                    ClientMessage::Authenticate(auth)=>{
                        if device.is_some(){break;}
                        if state.db(|s|s.authenticate(&auth,&nonce,&fingerprint)).is_err(){break;}
                        device=Some(auth.device_id.clone());send(&sender,ServerMessage::Authenticated{device_id:auth.device_id}).await;
                    },
                    ClientMessage::Request{id,method,params} if device.is_none()=>{
                        let result=match method.as_str(){
                            "device/register" if registration_attempts<5=>{registration_attempts+=1;match serde_json::from_value::<Registration>(params){Ok(request)=>state.db(|s|s.register(request,&fingerprint,&nonce)).and_then(|value|serde_json::to_value(value).map_err(error)),Err(e)=>Err(error(e))}},
                            "device/claim"=>{let id=DeviceId(params["deviceId"].as_str().unwrap_or_default().into());let signature=params["signature"].as_str().unwrap_or_default();state.db(|s|s.claim(&id,&nonce,&fingerprint,signature)).and_then(|value|serde_json::to_value(value).map_err(error))},
                            _=>Err(error("device authentication required"))
                        };
                        let response=match result{Ok(result)=>ServerMessage::Response{id,result},Err(e)=>ServerMessage::Error{id:Some(id),code:"registrationFailed".into(),message:e.to_string()}};
                        send(&sender,response).await;
                    },
                    ClientMessage::Request{id,method,params}=>{
                        let Ok(permit)=request_slots.clone().try_acquire_owned()else{send(&sender,ServerMessage::Error{id:Some(id),code:"overloaded".into(),message:"too many requests; retry later".into()}).await;continue;};
                        let state=state.clone();let sender=sender.clone();let Some(device)=device.clone()else{break;};
                        tokio::spawn(async move{let _permit=permit;let result=dispatch_recorded(&state,&device,&id,&method,params).await;let response=match result{Ok(result)=>ServerMessage::Response{id,result},Err(e)=>ServerMessage::Error{id:Some(id),code:"requestFailed".into(),message:e.to_string()}};send(&sender,response).await;});
                    },
                    ClientMessage::Subscribe{after}=>{
                        let Some(device)=device.clone()else{break;};if let Some(task)=subscription.take(){task.abort();}
                        subscription=Some(tokio::spawn(subscribe(state.clone(),sender.clone(),device,after)));
                    },
                    ClientMessage::Ping=>{send(&sender,ServerMessage::Pong).await;}
                }
            }
        }
    }
    if let Some(subscription) = subscription {
        subscription.abort();
    }
    writer.abort();
}

async fn subscribe(
    state: Arc<State>,
    sender: mpsc::Sender<ServerMessage>,
    device: DeviceId,
    mut after: u64,
) {
    let mut live = state.events.subscribe();
    loop {
        if !state.db(|s| s.active(&device)).unwrap_or(false) {
            return;
        }
        let Ok((oldest, latest)) = state.db(|s| s.bounds()) else {
            return;
        };
        if after > latest || (after > 0 && after.saturating_add(1) < oldest) {
            if !send(&sender, ServerMessage::Reset { oldest, latest }).await {
                return;
            }
            after = oldest.saturating_sub(1);
        }
        let Ok(events) = state.db(|s| s.events(after, 16)) else {
            return;
        };
        if !events.is_empty() {
            for event in events {
                after = event.sequence;
                if !send(&sender, ServerMessage::Event(event)).await {
                    return;
                }
            }
            continue;
        }
        match live.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn dispatch_recorded(
    state: &Arc<State>,
    device: &DeviceId,
    id: &RequestId,
    method: &str,
    params: Value,
) -> Result<Value> {
    if !state.db(|s| s.active(device))? {
        return Err(error("device was revoked"));
    }
    // Terminal bytes and typed passwords stay in bounded memory, never in the journal.
    if method.starts_with("terminal/") {
        return super::terminals::dispatch(state, device, method, &params).await;
    }
    // File chunks are immutable snapshot reads. Caching every chunk as a command
    // outcome would copy entire downloads into the request journal.
    if matches!(
        method,
        "file/download/chunk"
            | "file/status"
            | "session/list"
            | "task/active/list"
            | "device/list"
            | "project/list"
            | "history/list"
            | "directory/list"
            | "transport/peers"
            | "launcher/list"
            | "launcher/read"
    ) {
        return dispatch(state, device, method, params).await;
    }
    if let Some(result) =
        state.db(|s| s.begin_request(device, id, &json!({"method":method,"params":params})))?
    {
        return decode_outcome(&result);
    }
    let outcome = match dispatch(state, device, method, params).await {
        Ok(value) => json!({"status":"completed","value":value}),
        Err(failure) => json!({"status":"failed","message":failure.to_string()}),
    };
    // TOML errors can quote source text. Return that detail only to the caller.
    let recorded_outcome = if method == "launcher/write" && outcome["status"] == "failed" {
        json!({"status":"failed","message":"Settings could not be saved. Reload them before retrying."})
    } else {
        outcome.clone()
    };
    state.db(|s| s.finish_request(device, id, &recorded_outcome))?;
    if outcome["status"] == "completed" {
        let notification = match method {
            "project/add" | "project/open" | "project/createWork" => Some("project/changed"),
            "session/create" | "session/stop" | "session/restart" => Some("session/changed"),
            "device/revoke" => Some("device/changed"),
            "connectionPassword/rotate" => Some("connectionPassword/changed"),
            "launcher/write" => Some("launcher/changed"),
            _ => None,
        };
        if let Some(notification) = notification {
            // Catalog changes have no task owner. Keep them in the same authenticated,
            // replayable journal as task events so all connected devices can refresh.
            if let Err(failure) = state.publish(
                &codex_start_remote::SessionId(String::new()),
                &json!({"method":notification,"params":{}}),
            ) {
                tracing::warn!(%failure, "catalog change could not be published");
            }
        }
    }
    decode_outcome(&outcome)
}

fn decode_outcome(outcome: &Value) -> Result<Value> {
    if outcome["status"] == "completed" {
        Ok(outcome["value"].clone())
    } else {
        Err(error(
            outcome["message"].as_str().unwrap_or("delivery unknown"),
        ))
    }
}

pub(super) async fn codex_rpc(state: &Arc<State>, params: &Value) -> Result<Value> {
    let session = params["sessionId"].as_str().unwrap_or_default();
    if session.starts_with("history-") {
        return super::history::rpc(
            state,
            session,
            params["method"].as_str().unwrap_or_default(),
            &params["params"],
        )
        .await;
    }
    let backend =
        super::sessions::get(state, params["sessionId"].as_str().unwrap_or_default()).await?;
    let method = params["method"]
        .as_str()
        .ok_or_else(|| error("missing Codex method"))?;
    if method.starts_with("remoteControl/") {
        return Err(error("use codex-start device and connection controls"));
    }
    if matches!(method, "initialize" | "initialized") {
        return Err(error("session initialization is managed by the daemon"));
    }
    if method == "thread/snapshot" {
        return super::thread_access::snapshot(state, &backend.info.id.0, &params["params"]).await;
    }
    let backend = if let Some(thread) = params["params"]["threadId"].as_str() {
        super::sessions::for_thread(state, &backend.info.id.0, thread).await?
    } else {
        backend
    };
    if method == "thread/writeAccess" {
        let id = params["params"]["threadId"]
            .as_str()
            .ok_or_else(|| error("missing thread ID"))?;
        let loaded = backend.request("thread/loaded/list", json!({})).await?;
        if !loaded["data"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|thread| thread.as_str() == Some(id)))
        {
            backend
                .request("thread/resume", json!({"threadId":id,"excludeTurns":true}))
                .await?;
        }
        return Ok(json!({"sessionId":backend.info.id,"writable":true}));
    }
    backend.request(method, params["params"].clone()).await
}

async fn dispatch(
    state: &Arc<State>,
    device: &DeviceId,
    method: &str,
    params: Value,
) -> Result<Value> {
    if method.starts_with("file/") {
        return super::files::dispatch(state, device, method, &params).await;
    }
    match method {
        "launcher/list" | "launcher/read" | "launcher/write" => {
            super::settings::dispatch(state, method, &params).await
        }
        "project/list" => super::projects::list(state).await,
        "history/list" => super::history::list(state, params).await,
        "project/add" => super::projects::add(state, params["path"].as_str().unwrap_or_default())
            .map(|project| json!(project)),
        "project/createWork" => {
            super::projects::create_work_remote(
                state,
                params["workId"].as_str().unwrap_or_default(),
            )
            .await
        }
        "project/open" => {
            super::projects::open(
                state,
                params["projectId"].as_str().unwrap_or_default(),
                params["profile"].as_str(),
            )
            .await
        }
        "directory/list" => super::projects::directories(&params).await,
        "transport/peers" => {
            let overlay = state
                .overlay
                .lock()
                .map_err(|_| error("overlay lock poisoned"))?
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            let peers = if let Some(overlay) = overlay {
                overlay.active_peers().await
            } else {
                Vec::new()
            };
            Ok(json!({"data":peers}))
        }
        "session/list" => Ok(json!({"data":super::sessions::list(state).await})),
        "task/active/list" => Ok(super::sessions::active_tasks(state).await),
        "session/create" => super::sessions::create(state, &params).await,
        "session/stop" | "session/restart" | "session/logs" => {
            super::sessions::control_session(state, method, &params).await
        }
        "codex/rpc" => codex_rpc(state, &params).await,
        "codex/reply" => {
            super::sessions::get(state, params["sessionId"].as_str().unwrap_or_default())
                .await?
                .reply(params["id"].clone(), params["result"].clone())
                .await
        }
        "workspace/diff" => {
            let backend =
                super::sessions::get(state, params["sessionId"].as_str().unwrap_or_default())
                    .await?;
            let mut command = vec![
                "git",
                "diff",
                "--no-color",
                "--patch",
                "--no-ext-diff",
                "--no-textconv",
            ];
            let mode = params["mode"].as_str().unwrap_or("working");
            if mode == "staged" {
                command.push("--cached");
            } else if mode == "branch" {
                let base = params["base"]
                    .as_str()
                    .filter(|v| !v.starts_with('-'))
                    .ok_or_else(|| error("invalid base branch"))?;
                command.push(base);
            }
            command.push("--");
            backend.request("command/exec",json!({"command":command,"cwd":backend.info.execution_cwd,"outputBytesCap":2*1024*1024})).await
        }
        "device/list" => state
            .db(|s| s.devices())
            .map(|devices| json!({"data":devices})),
        "device/revoke" => {
            state.db(|s| {
                s.revoke(&DeviceId(
                    params["deviceId"].as_str().unwrap_or_default().into(),
                ))
            })?;
            Ok(json!({}))
        }
        "connectionPassword/rotate" => {
            state.db(codex_start_remote::store::Store::rotate_password)?;
            Ok(json!({}))
        }
        "transport/status" => Ok(
            json!({"yggdrasil":state.overlay_status.lock().map_err(|_|error("status lock poisoned"))?.clone(),"discovery":state.discovery}),
        ),
        _ => Err(error("unsupported gateway method")),
    }
}

async fn admin_loop(listener: tokio::net::UnixListener, state: Arc<State>) {
    while let Ok((stream, _)) = listener.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read).take(16384);
            let mut bytes = Vec::new();
            if tokio::time::timeout(Duration::from_secs(5), reader.read_until(b'\n', &mut bytes))
                .await
                .is_err()
            {
                return;
            }
            let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
                return;
            };
            let result = match request["method"].as_str().unwrap_or_default() {
                "status" => Ok(
                    json!({"status":"running","discovery":state.discovery,"yggdrasil":state.overlay_status.lock().map(|s|s.clone()).unwrap_or_default()}),
                ),
                "stop" => Ok(json!({"status":"stopping"})),
                "invitation" => state
                    .invitation(request["host"].as_str().map(str::to_owned))
                    .and_then(|i| i.encode().map_err(error))
                    .map(|uri| json!({"uri":uri})),
                "approve" => state
                    .db(|s| s.approve(request["code"].as_str().unwrap_or_default()))
                    .map(|()| json!({"approved":true})),
                "devices" => state
                    .db(|s| s.devices())
                    .map(|devices| json!({"data":devices})),
                "revoke" => state
                    .db(|s| s.revoke(&DeviceId(request["id"].as_str().unwrap_or_default().into())))
                    .map(|()| json!({"revoked":true})),
                "rotate" => state
                    .db(codex_start_remote::store::Store::rotate_password)
                    .map(|()| json!({"rotated":true})),
                _ => Err(error("unknown local control method")),
            };
            let response = result.unwrap_or_else(|e| json!({"error":e.to_string()}));
            let _ = write.write_all(format!("{response}\n").as_bytes()).await;
            if request["method"] == "stop" {
                state.stop.notify_one();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_start_client::Client;
    use codex_start_remote::{Invitation, crypto};

    async fn gateway() -> (tempfile::TempDir, Arc<State>, tokio::task::JoinHandle<()>) {
        let root = tempfile::tempdir().unwrap();
        let (tls, fingerprint) = codex_start_transport::tls::identity(root.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = DaemonOptions {
            bind: listener.local_addr().unwrap(),
            no_yggdrasil: true,
            advertise_host: None,
        };
        let mut state = State::open(
            root.path().to_owned(),
            Some(root.path().join("config.toml")),
            options,
            fingerprint,
        )
        .unwrap();
        Arc::get_mut(&mut state).unwrap().session_directory = root.path().join("sessions");
        let router = Router::new()
            .route("/v1/discovery", get(discovery))
            .route("/v1/ws", get(upgrade))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let router = router.clone();
                let tls = tls.clone();
                tokio::spawn(serve(Box::new(stream), router, tls));
            }
        });
        (root, state, task)
    }

    /// Test-only event source for the physical Android background test. It uses
    /// the production TLS, registration, journal, and subscription handlers.
    #[tokio::test]
    #[ignore = "started by scripts/test-android-background.py"]
    async fn android_notification_fixture() {
        let directory = PathBuf::from(
            std::env::var_os("CODEX_START_ANDROID_FIXTURE").expect("test fixture directory"),
        );
        let (_root, state, server) = gateway().await;
        let session = codex_start_remote::SessionId::generate();
        let invitation = state
            .invitation(Some("127.0.0.1".into()))
            .unwrap()
            .encode()
            .unwrap();
        atomic_write(&directory.join("connection.json"), &json!({"invitation":invitation,"port":state.options.bind.port(),"daemonId":state.discovery.daemon_id,"sessionId":session}).to_string()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        while !directory.join("stop").exists() && tokio::time::Instant::now() < deadline {
            if let Ok(bytes) = tokio::fs::read(directory.join("event.json")).await {
                let event: Value = serde_json::from_slice(&bytes).unwrap();
                state.publish(&session, &event).unwrap();
                tokio::fs::remove_file(directory.join("event.json"))
                    .await
                    .unwrap();
            }
            atomic_write(&directory.join("status.json"), &json!({"connections":64-state.connections.available_permits(),"sequence":state.db(|s|s.bounds()).unwrap().1}).to_string()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        server.abort();
    }

    fn navigation_project(root: &std::path::Path) -> PathBuf {
        let project_path = root.join("Navigation project");
        std::fs::create_dir_all(project_path.join("Nested folder")).unwrap();
        let project_path = std::fs::canonicalize(project_path).unwrap();
        std::fs::write(project_path.join("demo.txt"), "old\n").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "color.ui", "always"],
            vec!["add", "demo.txt"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&project_path)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        project_path
    }

    async fn prepare_work_fixture(
        state: &Arc<State>,
        codex: &str,
        socket: &std::path::Path,
        session: &codex_start_remote::SessionInfo,
    ) -> Value {
        let mut work_fixture = json!({});
        if std::env::var_os("CODEX_START_WORK_FIXTURE").is_some() {
            std::fs::write(
                state.config.as_ref().unwrap(),
                "[profiles.work.settings]\nnetwork='offline'\n",
            )
            .unwrap();
            let work_id = uuid::Uuid::new_v4().to_string();
            let project = super::super::projects::create_work(state, &work_id).unwrap();
            for profile in [None, Some("work")] {
                let info = codex_start_remote::SessionInfo {
                    id: codex_start_remote::SessionId::generate(),
                    name: "Work fixture".into(),
                    cwd: project.path.clone(),
                    execution_cwd: project.path.clone(),
                    profile: profile.map(str::to_owned),
                    ..session.clone()
                };
                if profile.is_some() {
                    work_fixture["sessionId"] = json!(info.id);
                }
                super::super::sessions::connect_fixture(
                    state,
                    codex_start_remote::SessionEndpoint {
                        info,
                        program: codex.to_owned(),
                        args: vec![
                            "app-server".into(),
                            "proxy".into(),
                            "--sock".into(),
                            socket.to_string_lossy().into(),
                        ],
                        owner_pid: None,
                    },
                )
                .await;
            }
            work_fixture["workId"] = json!(work_id);
            work_fixture["project"] = json!(project);
            work_fixture["root"] = json!(std::fs::canonicalize(&state.work_directory).unwrap());
        }
        work_fixture
    }

    #[tokio::test]
    #[ignore = "started by scripts/test-android-navigation.py with a local Codex binary"]
    async fn android_navigation_fixture() {
        let directory = PathBuf::from(std::env::var_os("CODEX_START_ANDROID_FIXTURE").unwrap());
        let codex = std::env::var("CODEX_START_TEST_CODEX").unwrap();
        let (root, state, server) = gateway().await;
        if std::env::var_os("CODEX_START_HISTORY_FIXTURE").is_some() {
            super::super::history::prepare_fixture(&state.root);
        }
        let project_path = navigation_project(root.path());
        let codex_home = root.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        if let Ok(url) = std::env::var("CODEX_START_TEST_MODEL_URL") {
            assert!(url.starts_with("http://127.0.0.1:"));
            std::fs::write(
                codex_home.join("config.toml"),
                format!(
                    "model = 'fixture'\nmodel_provider = 'fixture'\n[model_providers.fixture]\nname = 'Local queue fixture'\nbase_url = '{url}'\nwire_api = 'responses'\n"
                ),
            )
            .unwrap();
        }
        let socket = root.path().join("codex.sock");
        let mut process = tokio::process::Command::new(&codex)
            .args([
                "app-server",
                "--listen",
                &format!("unix://{}", socket.display()),
            ])
            .env("CODEX_HOME", codex_home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let session = codex_start_remote::SessionInfo {
            id: codex_start_remote::SessionId::generate(),
            name: "Navigation session".into(),
            cwd: project_path.to_string_lossy().into(),
            execution_cwd: project_path.to_string_lossy().into(),
            environment: "local test".into(),
            profile: None,
            status: "running".into(),
            kind: "persistent".into(),
            capabilities: vec!["codexRpc".into()],
        };
        let backend = super::super::sessions::connect_fixture(
            &state,
            codex_start_remote::SessionEndpoint {
                info: session.clone(),
                program: codex.clone(),
                args: vec![
                    "app-server".into(),
                    "proxy".into(),
                    "--sock".into(),
                    socket.to_string_lossy().into(),
                ],
                owner_pid: None,
            },
        )
        .await;
        let chat = backend
            .request(
                "thread/start",
                json!({"cwd":project_path,"historyMode":"paginated"}),
            )
            .await
            .unwrap();
        backend
            .request(
                "thread/name/set",
                json!({"threadId":chat["thread"]["id"],"name":"Review navigation changes"}),
            )
            .await
            .unwrap();
        super::super::projects::add(&state, project_path.to_str().unwrap()).unwrap();
        let work_fixture = prepare_work_fixture(&state, &codex, &socket, &session).await;
        let invitation = state
            .invitation(Some("127.0.0.1".into()))
            .unwrap()
            .encode()
            .unwrap();
        atomic_write(&directory.join("connection.json"), &json!({"invitation":invitation,"port":state.options.bind.port(),"daemonId":state.discovery.daemon_id,"hostname":state.discovery.name,"home":std::env::var("HOME").unwrap(),"sessionId":session.id,"projectPath":project_path,"threadId":chat["thread"]["id"],"work":work_fixture}).to_string()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        while !directory.join("stop").exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        process.kill().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    #[ignore = "started by scripts/test-android-navigation.py --container-fixture"]
    async fn android_terminal_container_fixture() {
        let directory = PathBuf::from(std::env::var_os("CODEX_START_ANDROID_FIXTURE").unwrap());
        let (root, state, server) = gateway().await;
        let project = root.path().join("Terminal project");
        std::fs::create_dir(&project).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let name = format!("cs-terminal-test-{}", uuid::Uuid::new_v4());
        struct TestContainer(String);
        impl Drop for TestContainer {
            fn drop(&mut self) {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", &self.0])
                    .output();
            }
        }
        let _container = TestContainer(name.clone());
        let started = std::process::Command::new("docker").args([
            "run", "--detach", "--rm", "--pull=never", "--network=none", "--name", &name,
            "--mount", &format!("type=bind,source={},target=/project", project.display()),
            "--env", "CODEX_HOME=/tmp/codex-fixture", "--entrypoint", "sh",
            "ghcr.io/cofob/codex-start-generic:v0.2.0-rc.1", "-c",
            "mkdir -p /tmp/codex-fixture; exec codex app-server --listen unix:///tmp/terminal-test.sock",
        ]).output().unwrap();
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if tokio::process::Command::new("docker")
                    .args(["exec", &name, "test", "-S", "/tmp/terminal-test.sock"])
                    .status()
                    .await
                    .unwrap()
                    .success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let session = codex_start_remote::SessionInfo {
            id: codex_start_remote::SessionId::generate(),
            name: "Container terminal fixture".into(),
            cwd: project.to_string_lossy().into(),
            execution_cwd: "/project".into(),
            environment: "generic".into(),
            profile: None,
            status: "running".into(),
            kind: "persistent".into(),
            capabilities: vec!["codexRpc".into()],
        };
        super::super::sessions::connect_fixture(
            &state,
            codex_start_remote::SessionEndpoint {
                info: session.clone(),
                program: "docker".into(),
                args: vec![
                    "exec".into(),
                    "-i".into(),
                    name,
                    "codex".into(),
                    "app-server".into(),
                    "proxy".into(),
                    "--sock".into(),
                    "/tmp/terminal-test.sock".into(),
                ],
                owner_pid: None,
            },
        )
        .await;
        let invitation = state
            .invitation(Some("127.0.0.1".into()))
            .unwrap()
            .encode()
            .unwrap();
        atomic_write(&directory.join("connection.json"), &json!({"invitation":invitation,"port":state.options.bind.port(),"daemonId":state.discovery.daemon_id,
            "hostname":state.discovery.name,"home":std::env::var("HOME").unwrap(),"sessionId":session.id,"projectPath":project,"executionCwd":"/project","threadId":""}).to_string()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        while !directory.join("stop").exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        super::super::terminals::reap(&state, true).await;
        server.abort();
    }

    #[tokio::test]
    async fn launcher_edits_do_not_record_settings_or_parser_excerpts() {
        let (_root, state, server) = gateway().await;
        let cache = tempfile::tempdir().unwrap();
        let invitation = state.invitation(None).unwrap().encode().unwrap();
        let first = Client::invitation(&invitation, cache.path()).await.unwrap();
        let registration = first.enroll("settings editor").await.unwrap();
        let device = registration.device_id;
        for (text, succeeds) in [
            ("# secret-value\n[settings]\nnetwork='bridge'\n", true),
            ("secret-value = [", false),
        ] {
            let current = dispatch(&state, &device, "launcher/read", json!({}))
                .await
                .unwrap();
            let id = RequestId::generate();
            let params = json!({"scope":"global","text":text,"version":current["version"]});
            let result =
                dispatch_recorded(&state, &device, &id, "launcher/write", params.clone()).await;
            assert_eq!(result.is_ok(), succeeds);
            let recorded = state
                .db(|store| {
                    store.begin_request(
                        &device,
                        &id,
                        &json!({"method":"launcher/write","params":params}),
                    )
                })
                .unwrap()
                .unwrap();
            assert!(!recorded.to_string().contains("secret-value"));
            assert_eq!(
                recorded["status"],
                if succeeds { "completed" } else { "failed" }
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn terminal_survives_transport_reconnect_without_journaling_bytes() {
        let (_root, state, server) = gateway().await;
        let cache = tempfile::tempdir().unwrap();
        let invitation = state.invitation(None).unwrap().encode().unwrap();
        let enrollment = Client::invitation(&invitation, cache.path()).await.unwrap();
        let device = enrollment.enroll("terminal test").await.unwrap().device_id;
        let saved = enrollment.saved().unwrap();
        drop(enrollment);
        let client = Client::resume(saved.clone(), cache.path()).await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let journal = state.db(|s| s.bounds()).unwrap();
        client
            .request(
                "terminal/open",
                json!({"terminalId":id,"target":"host","cwd":state.root,
            "command":"printf BEFORE; sleep 1; printf AFTER; exec /bin/sh"}),
            )
            .await
            .unwrap();
        drop(client);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let client = Client::resume(saved, cache.path()).await.unwrap();
        let result = client
            .request("terminal/read", json!({"terminalId":id,"offset":0}))
            .await
            .unwrap();
        assert!(
            String::from_utf8(
                base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    result["dataBase64"].as_str().unwrap()
                )
                .unwrap()
            )
            .unwrap()
            .contains("BEFOREAFTER")
        );
        assert_eq!(journal, state.db(|s| s.bounds()).unwrap());
        let request = RequestId::generate();
        let params = json!({"terminalId":id,"offset":0,"dataBase64":"cHdkDQ=="});
        dispatch_recorded(&state, &device, &request, "terminal/write", params.clone())
            .await
            .unwrap();
        assert!(
            state
                .db(|s| s.begin_request(
                    &device,
                    &request,
                    &json!({"method":"terminal/write","params":params})
                ))
                .unwrap()
                .is_none()
        );
        client
            .request("terminal/close", json!({"terminalId":id}))
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn catalog_changes_are_streamed_to_other_devices() {
        let (root, state, server) = gateway().await;
        let cache = tempfile::tempdir().unwrap();
        let invitation = state.invitation(None).unwrap().encode().unwrap();
        let editor_cache = cache.path().join("editor");
        let viewer_cache = cache.path().join("viewer");
        let first = Client::invitation(&invitation, &editor_cache)
            .await
            .unwrap();
        first.enroll("catalog editor").await.unwrap();
        let editor = Client::resume(first.saved().unwrap(), &editor_cache)
            .await
            .unwrap();
        let second = Client::invitation(&invitation, &viewer_cache)
            .await
            .unwrap();
        second.enroll("catalog viewer").await.unwrap();
        let viewer = Client::resume(second.saved().unwrap(), &viewer_cache)
            .await
            .unwrap();
        editor
            .request("project/add", json!({"path":root.path()}))
            .await
            .unwrap();
        let change = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ServerMessage::Event(event)) = viewer.next_event().await {
                    if event.message["method"] == "project/changed" {
                        break event;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(change.session_id.0.is_empty());
        assert!(viewer.pending_alerts().unwrap().is_empty());
        assert_eq!(
            viewer.request("project/list", json!({})).await.unwrap()["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        server.abort();
    }

    #[tokio::test]
    async fn large_files_round_trip_in_chunks_and_incomplete_uploads_do_not_replace_the_target() {
        use std::os::unix::fs::PermissionsExt;
        let (root, state, server) = gateway().await;
        let engine = root.path().join("fixture-engine");
        std::fs::write(&engine, "#!/bin/sh\nshift 3\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = codex_start_remote::SessionId::generate();
        super::super::sessions::register_fixture(
            &state,
            codex_start_remote::SessionEndpoint {
                info: codex_start_remote::SessionInfo {
                    id: id.clone(),
                    name: "file fixture".into(),
                    cwd: root.path().to_string_lossy().into(),
                    execution_cwd: root.path().to_string_lossy().into(),
                    environment: "fixture".into(),
                    profile: None,
                    status: "running".into(),
                    kind: "fixture".into(),
                    capabilities: vec!["files".into()],
                },
                program: engine.to_string_lossy().into(),
                args: vec!["exec".into(), "-i".into(), "fixture".into(), "codex".into()],
                owner_pid: None,
            },
        );
        let cache = tempfile::tempdir().unwrap();
        let registered = Client::invitation(
            &state.invitation(None).unwrap().encode().unwrap(),
            cache.path(),
        )
        .await
        .unwrap();
        registered.enroll("file phone").await.unwrap();
        let phone = Client::resume(registered.saved().unwrap(), cache.path())
            .await
            .unwrap();
        let input = root.path().join("input");
        let target = root.path().join("target with spaces");
        let downloaded = root.path().join("downloaded");
        let data: Vec<u8> = (0..6 * 1024 * 1024)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        std::fs::write(&input, &data).unwrap();
        phone
            .upload_file(&id.0, &input, target.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), data);
        phone
            .download_file(&id.0, target.to_str().unwrap(), &downloaded)
            .await
            .unwrap();
        assert_eq!(std::fs::read(downloaded).unwrap(), data);
        let preview = root.path().join("bounded-preview");
        assert!(
            phone
                .download_file_limited(&id.0, target.to_str().unwrap(), &preview, 1024 * 1024)
                .await
                .is_err()
        );
        assert!(
            !preview.exists(),
            "an oversized preview must fail before local creation"
        );
        let partial = phone
            .request(
                "file/upload/start",
                json!({"sessionId":id,"path":target,"size":3,"sha256":crypto::digest(b"new")}),
            )
            .await
            .unwrap();
        assert!(
            phone
                .request(
                    "file/upload/finish",
                    json!({"transferId":partial["transferId"]})
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), data);
        server.abort();
    }

    #[tokio::test]
    async fn qr_manual_pairing_rotation_revocation_and_replay_over_pinned_tls() {
        let (_root, state, server) = gateway().await;
        let cache = tempfile::tempdir().unwrap();
        let invite = state.invitation(None).unwrap();
        let invitation = invite.encode().unwrap();
        let first = Client::invitation(&invitation, cache.path()).await.unwrap();
        let registration = first.enroll("test phone").await.unwrap();
        assert!(registration.token.is_some());
        let saved = first.saved().unwrap();
        drop(first);
        let phone = Client::resume(saved.clone(), cache.path()).await.unwrap();
        assert_eq!(
            phone.request("session/list", json!({})).await.unwrap()["data"],
            json!([])
        );
        assert_eq!(
            phone.request("task/active/list", json!({})).await.unwrap(),
            json!({"data":[],"unavailableSessions":[]})
        );
        let session = codex_start_remote::SessionId::generate();
        let completed = json!({"method":"turn/completed","params":{"turn":{"id":"turn-1","status":"completed"},"threadId":"thread-1"}});
        state.publish(&session, &completed).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ServerMessage::Event(event)) = phone.next_event().await {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(event.message, completed);
        assert_eq!(phone.pending_alerts().unwrap().len(), 1);
        let reconnected = Client::resume(saved.clone(), cache.path()).await.unwrap();
        assert_eq!(
            reconnected.pending_alerts().unwrap()[0].sequence,
            event.sequence
        );
        reconnected.acknowledge_alert(event.sequence).unwrap();
        assert!(phone.pending_alerts().unwrap().is_empty());
        state
            .db(codex_start_remote::store::Store::rotate_password)
            .unwrap();
        assert!(phone.request("session/list", json!({})).await.is_ok());
        let expired_invitation = Client::invitation(&invitation, cache.path()).await.unwrap();
        assert!(expired_invitation.enroll("old invitation").await.is_err());
        let manual = Client::direct("127.0.0.1", state.options.bind.port(), cache.path())
            .await
            .unwrap();
        let request = manual.enroll("manual phone").await.unwrap();
        assert!(!manual.claim().await.unwrap());
        state
            .db(|s| s.approve(request.code.as_deref().unwrap()))
            .unwrap();
        assert!(manual.claim().await.unwrap());
        assert!(manual.saved().unwrap().token.is_some());
        let wrong = Invitation {
            fingerprint: Some(crypto::digest(b"wrong identity")),
            ..invite
        };
        assert!(
            Client::invitation(&wrong.encode().unwrap(), cache.path())
                .await
                .is_err()
        );
        state.db(|s| s.revoke(&registration.device_id)).unwrap();
        assert!(phone.request("session/list", json!({})).await.is_err());
        assert!(Client::resume(saved, cache.path()).await.is_err());
        server.abort();
    }
}
