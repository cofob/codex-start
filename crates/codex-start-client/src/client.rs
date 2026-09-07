//! Native remote client with pinned identity, request correlation, and event recovery.

use crate::Error;
use codex_start_remote::{
    Authentication, ClientMessage, Connection, DeviceId, Discovery, Invitation, Registration,
    RegistrationResult, RequestId, ServerMessage, crypto, store::Store,
};
use codex_start_transport::{Stream, overlay::Overlay};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedServer {
    pub discovery: Discovery,
    pub connection: Connection,
    pub device_secret: String,
    pub device_id: Option<DeviceId>,
    pub token: Option<String>,
    #[serde(default)]
    pub initial_cursor: u64,
}

type PendingRequests = Arc<Mutex<BTreeMap<String, oneshot::Sender<Result<Value, Error>>>>>;

pub struct Client {
    saved: Mutex<SavedServer>,
    nonce: String,
    store: Arc<Mutex<Store>>,
    invitation_password: Option<String>,
    outgoing: mpsc::Sender<ClientMessage>,
    pending: PendingRequests,
    events: tokio::sync::Mutex<mpsc::Receiver<ServerMessage>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    overlay: Option<Arc<Overlay>>,
}

impl Client {
    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn invitation(uri: &str, cache: &Path) -> Result<Arc<Self>, Error> {
        let invitation = Invitation::decode(uri)?;
        let discovery = Discovery {
            protocol_version: invitation.version,
            daemon_id: codex_start_remote::DaemonId(String::new()),
            name: String::new(),
            fingerprint: invitation.fingerprint.unwrap_or_default(),
            capabilities: Vec::new(),
        };
        let saved = SavedServer {
            discovery,
            connection: invitation.connection,
            device_secret: crypto::random_secret(),
            device_id: None,
            token: None,
            initial_cursor: 0,
        };
        Self::open(saved, Some(invitation.connection_password), cache, true).await
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn direct(host: &str, port: u16, cache: &Path) -> Result<Arc<Self>, Error> {
        let discovery = Discovery {
            protocol_version: codex_start_remote::PROTOCOL_VERSION,
            daemon_id: codex_start_remote::DaemonId(String::new()),
            name: host.into(),
            fingerprint: String::new(),
            capabilities: Vec::new(),
        };
        Self::open(
            SavedServer {
                discovery,
                connection: Connection::Direct {
                    host: host.into(),
                    port,
                },
                device_secret: crypto::random_secret(),
                device_id: None,
                token: None,
                initial_cursor: 0,
            },
            None,
            cache,
            false,
        )
        .await
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn resume(saved: SavedServer, cache: &Path) -> Result<Arc<Self>, Error> {
        Self::open(saved, None, cache, true).await
    }

    async fn open(
        saved: SavedServer,
        password: Option<String>,
        cache: &Path,
        pinned: bool,
    ) -> Result<Arc<Self>, Error> {
        Self::open_with_overlay(saved, password, cache, pinned, None).await
    }

    /// Authenticate a paired device while retaining its established Yggdrasil routes.
    pub async fn activate(&self, cache: &Path) -> Result<Arc<Self>, Error> {
        Self::open_with_overlay(self.saved()?, None, cache, true, self.overlay.clone()).await
    }

    async fn open_with_overlay(
        mut saved: SavedServer,
        password: Option<String>,
        cache: &Path,
        pinned: bool,
        existing: Option<Arc<Overlay>>,
    ) -> Result<Arc<Self>, Error> {
        tokio::fs::create_dir_all(cache).await?;
        let (transport, overlay) = if let (
            Some(network),
            Connection::Yggdrasil {
                public_key, port, ..
            },
        ) = (existing, &saved.connection)
        {
            (network.connect(public_key, *port).await?, Some(network))
        } else {
            connect_transport(&saved, cache).await?
        };
        let (mut socket, nonce) = authenticate_socket(transport, &mut saved, pinned).await?;
        let store = Arc::new(Mutex::new(Store::open(
            &cache.join(format!("{}.db", saved.discovery.daemon_id.0)),
        )?));
        let cursor = store
            .lock()
            .map_err(|_| Error::Message("cache lock failed".into()))?
            .value("remote_cursor")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(saved.initial_cursor);
        if saved.token.is_some() {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&ClientMessage::Subscribe { after: cursor })?.into(),
                ))
                .await?;
        }
        let (mut writer, reader) = socket.split();
        let (outgoing, mut receiver) = mpsc::channel::<ClientMessage>(64);
        let (event_sender, events) = mpsc::channel(8);
        let pending: PendingRequests = Arc::new(Mutex::new(BTreeMap::new()));
        let writer_task = tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                let Ok(text) = serde_json::to_string(&message) else {
                    break;
                };
                if writer
                    .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let requests = pending.clone();
        let reset_sender = outgoing.clone();
        let inbox = store.clone();
        let reader_task = tokio::spawn(read_socket(
            reader,
            requests,
            reset_sender,
            inbox,
            event_sender,
        ));
        let keepalive = outgoing.clone();
        let heartbeat = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticks.tick().await;
                if keepalive.send(ClientMessage::Ping).await.is_err() {
                    break;
                }
            }
        });
        let mut tasks = vec![writer_task, reader_task, heartbeat];
        if saved.token.is_some()
            && saved
                .discovery
                .capabilities
                .iter()
                .any(|value| value == "hostPeers")
            && let Some(network) = overlay.clone()
            && let Some(path) = host_peer_cache(&saved, cache)?
        {
            tasks.push(tokio::spawn(learn_host_peers(
                outgoing.clone(),
                pending.clone(),
                network,
                path,
            )));
        }
        Ok(Arc::new(Self {
            saved: Mutex::new(saved),
            nonce,
            store,
            invitation_password: password,
            outgoing,
            pending,
            events: tokio::sync::Mutex::new(events),
            tasks,
            overlay,
        }))
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub fn saved(&self) -> Result<SavedServer, Error> {
        Ok(self
            .saved
            .lock()
            .map_err(|_| Error::Message("connection lock failed".into()))?
            .clone())
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn enroll(&self, name: &str) -> Result<RegistrationResult, Error> {
        let saved = self.saved()?;
        let result: RegistrationResult = serde_json::from_value(
            self.request(
                "device/register",
                serde_json::to_value(Registration {
                    name: name.into(),
                    public_key: crypto::public_key(&saved.device_secret)?,
                    connection_password: self.invitation_password.clone(),
                })?,
            )
            .await?,
        )?;
        if let Some(code) = &result.code {
            let expected = crypto::approval_code(
                &crypto::public_key(&saved.device_secret)?,
                &saved.discovery.fingerprint,
                &self.nonce,
            );
            if *code != expected {
                return Err(Error::Message("pairing identity does not match".into()));
            }
        }
        self.accept_registration(&result)?;
        Ok(result)
    }

    fn accept_registration(&self, result: &RegistrationResult) -> Result<(), Error> {
        let mut saved = self
            .saved
            .lock()
            .map_err(|_| Error::Message("connection lock failed".into()))?;
        saved.device_id = Some(result.device_id.clone());
        saved.initial_cursor = result.event_cursor;
        if let Some(token) = &result.token {
            saved.token = Some(token.clone());
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn claim(&self) -> Result<bool, Error> {
        let saved = self.saved()?;
        let id = saved
            .device_id
            .ok_or_else(|| Error::Message("start registration first".into()))?;
        let result:Option<RegistrationResult>=serde_json::from_value(self.request("device/claim",json!({"deviceId":id,"signature":crypto::sign(&saved.device_secret,&self.nonce,&saved.discovery.fingerprint)?})).await?)?;
        if let Some(result) = result {
            self.accept_registration(&result)?;
            return Ok(true);
        }
        Ok(false)
    }

    ///
    /// # Errors
    /// Returns an error if the connection, identity check, protocol operation, or local storage fails.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let persistent_terminal = method == "codex/rpc"
            && params["method"] == "command/exec"
            && params["params"]["tty"] == true
            && params["params"]["disableTimeout"] == true;
        let id = RequestId::generate();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| Error::Message("request lock failed".into()))?
            .insert(id.0.clone(), tx);
        if self
            .outgoing
            .send(ClientMessage::Request {
                id: id.clone(),
                method: method.into(),
                params,
            })
            .await
            .is_err()
        {
            self.pending
                .lock()
                .map_err(|_| Error::Message("request lock failed".into()))?
                .remove(&id.0);
            return Err(Error::Message("connection closed".into()));
        }
        let result = if persistent_terminal {
            Ok(rx.await)
        } else {
            tokio::time::timeout(Duration::from_secs(650), rx).await
        };
        self.pending
            .lock()
            .map_err(|_| Error::Message("request lock failed".into()))?
            .remove(&id.0);
        result
            .map_err(|_| {
                Error::Message("delivery unknown; inspect the task before retrying".into())
            })?
            .map_err(|_| Error::Message("connection lost; delivery unknown".into()))?
    }

    /// Read notifications that still need Android delivery.
    ///
    /// # Errors
    /// Returns an error if the local cache is unavailable.
    pub fn pending_alerts(&self) -> Result<Vec<codex_start_remote::Event>, Error> {
        Ok(self
            .store
            .lock()
            .map_err(|_| Error::Message("cache lock failed".into()))?
            .alerts()?)
    }

    /// Confirm that Android posted an alert.
    ///
    /// # Errors
    /// Returns an error if the cache cannot save the acknowledgement.
    pub fn acknowledge_alert(&self, sequence: u64) -> Result<(), Error> {
        Ok(self
            .store
            .lock()
            .map_err(|_| Error::Message("cache lock failed".into()))?
            .acknowledge_alert(sequence)?)
    }

    pub async fn next_event(&self) -> Option<ServerMessage> {
        match tokio::time::timeout(Duration::from_secs(5), self.events.lock().await.recv()).await {
            Ok(None) => Some(ServerMessage::Error {
                id: None,
                code: "disconnected".into(),
                message: "connection closed".into(),
            }),
            Ok(message) => message,
            Err(_) => None,
        }
    }
    pub async fn network_changed(&self) {
        if let Some(overlay) = &self.overlay {
            overlay.network_changed().await;
        }
    }
    pub async fn transport_peers(&self) -> Vec<crate::PeerInfo> {
        if let Some(overlay) = &self.overlay {
            overlay.peer_info().await
        } else {
            Vec::new()
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn read_message(
    socket: &mut tokio_tungstenite::WebSocketStream<Stream>,
) -> Result<ServerMessage, Error> {
    let frame = tokio::time::timeout(Duration::from_secs(20), socket.next())
        .await
        .map_err(|_| Error::Message("server response timed out".into()))?
        .ok_or_else(|| Error::Message("connection closed".into()))??;
    let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
        return Err(Error::Message("unexpected server response".into()));
    };
    Ok(serde_json::from_str(&text)?)
}

pub async fn discover(host: &str, port: Option<u16>, cache: PathBuf) -> Vec<SavedServer> {
    let ports: Vec<_> = port.map_or_else(
        || (codex_start_remote::DEFAULT_PORT..=codex_start_remote::LAST_DISCOVERY_PORT).collect(),
        |p| vec![p],
    );
    futures::stream::iter(ports)
        .map(|port| {
            let cache = cache.clone();
            async move {
                let client = tokio::time::timeout(
                    Duration::from_secs(2),
                    Client::direct(host, port, &cache),
                )
                .await
                .ok()?
                .ok()?;
                client.saved().ok()
            }
        })
        .buffer_unordered(4)
        .filter_map(|v| async move { v })
        .collect()
        .await
}

fn host_peer_cache(saved: &SavedServer, cache: &Path) -> Result<Option<PathBuf>, Error> {
    match &saved.connection {
        Connection::Yggdrasil { public_key, .. } => {
            let key = crypto::decode32(public_key)?;
            Ok(Some(
                cache.join(format!("host-peers-{}.json", hex::encode(key))),
            ))
        }
        Connection::Direct { .. } => Ok(None),
    }
}

async fn connect_transport(
    saved: &SavedServer,
    cache: &Path,
) -> Result<(Stream, Option<Arc<Overlay>>), Error> {
    let mut overlay = None;
    let transport: Stream = match &saved.connection {
        Connection::Direct { host, port } => Box::new(
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .map_err(|_| Error::Message("host connection timed out".into()))??,
        ),
        Connection::Yggdrasil {
            public_key,
            group_password,
            port,
        } => {
            let path = host_peer_cache(saved, cache)?.expect("Yggdrasil peer cache path");
            let preferred = codex_start_transport::peers::cached_host(&path).await;
            let network = Arc::new(
                Overlay::start_client(&saved.device_secret, group_password, &preferred).await?,
            );
            let stream = network.connect(public_key, *port).await?;
            overlay = Some(network);
            stream
        }
    };
    Ok((transport, overlay))
}

async fn read_socket(
    mut reader: futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<Stream>>,
    requests: PendingRequests,
    reset_sender: mpsc::Sender<ClientMessage>,
    inbox: Arc<Mutex<Store>>,
    event_sender: mpsc::Sender<ServerMessage>,
) {
    // Three missed heartbeat intervals indicate a stale route, even if TCP stays open.
    while let Ok(Some(Ok(frame))) =
        tokio::time::timeout(Duration::from_secs(90), reader.next()).await
    {
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else {
            continue;
        };
        match message {
            ServerMessage::Response { id, result } => {
                if let Ok(mut pending) = requests.lock()
                    && let Some(tx) = pending.remove(&id.0)
                {
                    let _ = tx.send(Ok(result));
                }
            }
            ServerMessage::Error {
                id: Some(id),
                message,
                ..
            } => {
                if let Ok(mut pending) = requests.lock()
                    && let Some(tx) = pending.remove(&id.0)
                {
                    let _ = tx.send(Err(Error::Message(message)));
                }
            }
            ServerMessage::Event(ref event) => {
                let persisted = inbox.lock().ok().map(|mut db| db.receive_event(event));
                match persisted {
                    Some(Ok(true)) => {}
                    Some(Ok(false)) => continue,
                    _ => break,
                }
                if event_sender.send(message).await.is_err() {
                    break;
                }
            }
            ServerMessage::Reset { oldest, latest } => {
                if let Ok(db) = inbox.lock() {
                    let _ = db.set_value("remote_cursor", &oldest.saturating_sub(1).to_string());
                }
                let _ = event_sender
                    .send(ServerMessage::Reset { oldest, latest })
                    .await;
                let _ = reset_sender
                    .send(ClientMessage::Subscribe {
                        after: oldest.saturating_sub(1),
                    })
                    .await;
            }
            message => {
                if event_sender.send(message).await.is_err() {
                    break;
                }
            }
        }
    }
    if let Ok(mut pending) = requests.lock() {
        pending.clear();
    }
    let _ = event_sender
        .send(ServerMessage::Error {
            id: None,
            code: "disconnected".into(),
            message: "connection lost; reconnect to recover session state".into(),
        })
        .await;
}

fn tls_pin(saved: &SavedServer, pinned: bool) -> Option<String> {
    // Overlay::connect routes to the configured public key. The packet API verifies
    // the authenticated sender key against each IPv6 source address before delivery.
    // Direct TCP has no such peer authentication and must use its saved TLS pin.
    (pinned && matches!(saved.connection, Connection::Direct { .. }))
        .then(|| saved.discovery.fingerprint.clone())
}

async fn authenticate_socket(
    transport: Stream,
    saved: &mut SavedServer,
    pinned: bool,
) -> Result<(tokio_tungstenite::WebSocketStream<Stream>, String), Error> {
    let (stream, fingerprint) = tokio::time::timeout(
        Duration::from_secs(20),
        codex_start_transport::tls::connect(transport, tls_pin(saved, pinned)),
    )
    .await
    .map_err(|_| Error::Message("TLS connection timed out".into()))??;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(codex_start_remote::MAX_MESSAGE_BYTES));
    let (mut socket, _) =
        tokio_tungstenite::client_async_with_config("wss://cs.fob.wtf/v1/ws", stream, Some(config))
            .await?;
    let challenge = read_message(&mut socket).await?;
    let ServerMessage::Challenge {
        nonce,
        discovery,
        fingerprint: advertised,
    } = challenge
    else {
        return Err(Error::Message("missing server challenge".into()));
    };
    if advertised != fingerprint
        || discovery.fingerprint != fingerprint
        || discovery.protocol_version != codex_start_remote::PROTOCOL_VERSION
        || (pinned
            && !saved.discovery.daemon_id.0.is_empty()
            && saved.discovery.daemon_id != discovery.daemon_id)
    {
        return Err(Error::Message(
            "server identity or protocol does not match".into(),
        ));
    }
    if !discovery
        .daemon_id
        .0
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || discovery.daemon_id.0.len() != 36
    {
        return Err(Error::Message("invalid daemon identity".into()));
    }
    saved.discovery = discovery;
    if let (Some(id), Some(token)) = (&saved.device_id, &saved.token) {
        let auth = Authentication {
            device_id: id.clone(),
            token: token.clone(),
            signature: crypto::sign(&saved.device_secret, &nonce, &fingerprint)?,
        };
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&ClientMessage::Authenticate(auth))?.into(),
            ))
            .await?;
        if !matches!(
            read_message(&mut socket).await?,
            ServerMessage::Authenticated { .. }
        ) {
            return Err(Error::Message("device was not authenticated".into()));
        }
    }
    Ok((socket, nonce))
}

// Fetch addresses after the connection is usable. Use normal request correlation
// so unsolicited events cannot be mistaken for the peer response.
async fn learn_host_peers(
    outgoing: mpsc::Sender<ClientMessage>,
    pending: PendingRequests,
    overlay: Arc<Overlay>,
    path: PathBuf,
) {
    let id = RequestId::generate();
    let (tx, rx) = oneshot::channel();
    if let Ok(mut requests) = pending.lock() {
        requests.insert(id.0.clone(), tx);
    } else {
        return;
    }
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        outgoing
            .send(ClientMessage::Request {
                id: id.clone(),
                method: "transport/peers".into(),
                params: json!({}),
            })
            .await
            .ok()?;
        rx.await.ok()?.ok()
    })
    .await
    .ok()
    .flatten();
    if let Ok(mut requests) = pending.lock() {
        requests.remove(&id.0);
    }
    let Some(result) = result else {
        return;
    };
    let peers: Vec<_> = result["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(8)
        .map(str::to_owned)
        .collect();
    codex_start_transport::peers::remember_host(&path, &peers).await;
    overlay.prefer_peers(&peers).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn background_peer_reply_is_saved_and_removed_from_pending_requests() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("host.json");
        let network = Arc::new(
            Overlay::with_peers(
                &crypto::random_secret(),
                &crypto::random_secret(),
                None,
                &codex_start_transport::peers::PeerList {
                    source: "test".into(),
                    revision: "1".into(),
                    peers: vec![],
                },
                false,
            )
            .await
            .unwrap(),
        );
        let (outgoing, mut receiver) = mpsc::channel(8);
        let pending: PendingRequests = Arc::new(Mutex::new(BTreeMap::new()));
        let task = tokio::spawn(learn_host_peers(
            outgoing,
            pending.clone(),
            network,
            path.clone(),
        ));
        let message = receiver.recv().await.unwrap();
        let ClientMessage::Request { id, method, .. } = message else {
            panic!("expected peer request")
        };
        assert_eq!(method, "transport/peers");
        let sender = pending.lock().unwrap().remove(&id.0).unwrap();
        sender
            .send(Ok(
                json!({"data":["tcp://127.0.0.1:1","unix:///tmp/invalid"]}),
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            codex_start_transport::peers::cached_host(&path).await,
            vec!["tcp://127.0.0.1:1"]
        );
        assert!(pending.lock().unwrap().is_empty());
        task.abort();
        let _ = task.await;
    }
}
