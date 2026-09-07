//! `UniFFI` boundary. Generated ABI scaffolding is isolated in this crate.

// UniFFI exported arguments must own their values across the ABI.
#![allow(clippy::needless_pass_by_value)]

use codex_start_client::{Client, SavedServer};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

uniffi::setup_scaffolding!();

#[derive(uniffi::Record)]
pub struct NativeNetworkInterface {
    pub name: String,
    pub index: u32,
    pub addresses: Vec<String>,
}

#[derive(uniffi::Record)]
pub struct NativePeer {
    pub public_key: String,
    pub address: String,
    pub active: bool,
    pub inbound: bool,
    pub rtt_ms: f64,
    pub received_bytes: u64,
    pub sent_bytes: u64,
}

#[uniffi::export]
pub fn update_network_interfaces(interfaces: Vec<NativeNetworkInterface>) {
    codex_start_client::set_network_interfaces(
        interfaces
            .into_iter()
            .take(16)
            .filter(|interface| interface.index > 0)
            .map(|interface| codex_start_client::NetworkInterface {
                name: interface.name,
                index: interface.index,
                addrs: interface
                    .addresses
                    .iter()
                    .take(16)
                    .filter_map(|address| address.split('%').next()?.parse().ok())
                    .collect(),
            })
            .collect(),
    );
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    #[error("{reason}")]
    Failed { reason: String },
}
fn failure(error: impl std::fmt::Display) -> ClientError {
    ClientError::Failed {
        reason: error.to_string(),
    }
}
fn runtime() -> Result<&'static tokio::runtime::Runtime, ClientError> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, std::io::Error>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(failure)
}

#[derive(uniffi::Enum)]
pub enum EventKind {
    Message,
    Reconcile,
    Disconnected,
}

#[derive(uniffi::Record)]
pub struct NativeEvent {
    pub kind: EventKind,
    pub session_id: String,
    pub sequence: u64,
    /// A Codex app-server message inside the typed session envelope.
    pub message: String,
}
impl From<codex_start_remote::Event> for NativeEvent {
    fn from(event: codex_start_remote::Event) -> Self {
        Self {
            kind: EventKind::Message,
            session_id: event.session_id.0,
            sequence: event.sequence,
            message: event.message.to_string(),
        }
    }
}

#[derive(uniffi::Object)]
pub struct NativeClient {
    client: Arc<Client>,
}

#[uniffi::export]
impl NativeClient {
    #[uniffi::constructor]
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn from_invitation(uri: String, cache_directory: String) -> Result<Arc<Self>, ClientError> {
        Ok(Arc::new(Self {
            client: runtime()?
                .block_on(Client::invitation(&uri, &PathBuf::from(cache_directory)))
                .map_err(failure)?,
        }))
    }
    #[uniffi::constructor]
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn direct(
        host: String,
        port: u16,
        cache_directory: String,
    ) -> Result<Arc<Self>, ClientError> {
        Ok(Arc::new(Self {
            client: runtime()?
                .block_on(Client::direct(&host, port, &PathBuf::from(cache_directory)))
                .map_err(failure)?,
        }))
    }
    #[uniffi::constructor]
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn restore(
        saved_connection: String,
        cache_directory: String,
    ) -> Result<Arc<Self>, ClientError> {
        let saved: SavedServer = serde_json::from_str(&saved_connection).map_err(failure)?;
        Ok(Arc::new(Self {
            client: runtime()?
                .block_on(Client::resume(saved, &PathBuf::from(cache_directory)))
                .map_err(failure)?,
        }))
    }
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn enroll(&self, device_name: String) -> Result<String, ClientError> {
        let result = runtime()?
            .block_on(self.client.enroll(&device_name))
            .map_err(failure)?;
        serde_json::to_string(&result).map_err(failure)
    }
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn claim(&self) -> Result<bool, ClientError> {
        runtime()?.block_on(self.client.claim()).map_err(failure)
    }
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn saved_connection(&self) -> Result<String, ClientError> {
        serde_json::to_string(&self.client.saved().map_err(failure)?).map_err(failure)
    }
    /// Authenticate without restarting the paired device's overlay.
    pub fn activate(&self, cache_directory: String) -> Result<Arc<Self>, ClientError> {
        Ok(Arc::new(Self {
            client: runtime()?
                .block_on(self.client.activate(&PathBuf::from(cache_directory)))
                .map_err(failure)?,
        }))
    }
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn request(&self, method: String, parameters: String) -> Result<String, ClientError> {
        let params = serde_json::from_str(&parameters).map_err(failure)?;
        serde_json::to_string(
            &runtime()?
                .block_on(self.client.request(&method, params))
                .map_err(failure)?,
        )
        .map_err(failure)
    }
    ///
    /// # Errors
    /// Returns an error if the native runtime or remote client operation fails.
    pub fn next_event(&self) -> Result<Option<NativeEvent>, ClientError> {
        use codex_start_remote::ServerMessage;
        Ok(match runtime()?.block_on(self.client.next_event()) {
            Some(ServerMessage::Event(event)) => Some(event.into()),
            Some(ServerMessage::Reset { .. }) => Some(NativeEvent {
                kind: EventKind::Reconcile,
                session_id: String::new(),
                sequence: 0,
                message: String::new(),
            }),
            Some(ServerMessage::Error { message, .. }) => Some(NativeEvent {
                kind: EventKind::Disconnected,
                session_id: String::new(),
                sequence: 0,
                message,
            }),
            _ => None,
        })
    }
    /// Read alerts retained until Android confirms notification delivery.
    ///
    /// # Errors
    /// Returns an error if the native cache is unavailable.
    pub fn pending_alerts(&self) -> Result<Vec<NativeEvent>, ClientError> {
        Ok(self
            .client
            .pending_alerts()
            .map_err(failure)?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    /// Confirm notification delivery.
    ///
    /// # Errors
    /// Returns an error if the acknowledgement cannot be stored.
    pub fn acknowledge_alert(&self, sequence: u64) -> Result<(), ClientError> {
        self.client.acknowledge_alert(sequence).map_err(failure)
    }
    /// Refresh transport links after a network change.
    ///
    /// # Errors
    /// Returns an error if the native runtime cannot start.
    pub fn upload_file(
        &self,
        session_id: String,
        source: String,
        destination: String,
    ) -> Result<(), ClientError> {
        runtime()?
            .block_on(
                self.client
                    .upload_file(&session_id, &PathBuf::from(source), &destination),
            )
            .map_err(failure)
    }
    /// Download a verified snapshot to an unused local path.
    ///
    /// # Errors
    /// Returns an error if file transfer or checksum validation fails.
    pub fn download_file(
        &self,
        session_id: String,
        source: String,
        destination: String,
    ) -> Result<(), ClientError> {
        runtime()?
            .block_on(
                self.client
                    .download_file(&session_id, &source, &PathBuf::from(destination)),
            )
            .map_err(failure)
    }
    /// Download a bounded preview without loading a large file into the Android heap.
    ///
    /// # Errors
    /// Returns an error if the file exceeds the limit or the transfer fails.
    pub fn download_file_limited(
        &self,
        session_id: String,
        source: String,
        destination: String,
        maximum_bytes: u64,
    ) -> Result<(), ClientError> {
        runtime()?
            .block_on(self.client.download_file_limited(
                &session_id,
                &source,
                &PathBuf::from(destination),
                maximum_bytes,
            ))
            .map_err(failure)
    }
    /// Refresh links after a network change.
    ///
    /// # Errors
    /// Returns an error if the runtime is unavailable.
    pub fn network_changed(&self) -> Result<(), ClientError> {
        runtime()?.block_on(self.client.network_changed());
        Ok(())
    }
    /// Read link status without exposing group passwords or device tokens.
    ///
    /// # Errors
    /// Returns an error if the native runtime is unavailable.
    pub fn transport_peers(&self) -> Result<Vec<NativePeer>, ClientError> {
        Ok(runtime()?
            .block_on(self.client.transport_peers())
            .into_iter()
            .map(|peer| NativePeer {
                public_key: peer.public_key,
                address: peer.address,
                active: peer.active,
                inbound: peer.inbound,
                rtt_ms: peer.rtt_ms,
                received_bytes: peer.received_bytes,
                sent_bytes: peer.sent_bytes,
            })
            .collect())
    }
}

#[uniffi::export]
///
/// # Errors
/// Returns an error if the native runtime or remote client operation fails.
pub fn discover_host(
    host: String,
    port: Option<u16>,
    cache_directory: String,
) -> Result<Vec<String>, ClientError> {
    let found = runtime()?.block_on(codex_start_client::discover(
        &host,
        port,
        PathBuf::from(cache_directory),
    ));
    found
        .into_iter()
        .map(|entry| {
            serde_json::to_string(
                &serde_json::json!({"discovery":entry.discovery,"connection":entry.connection}),
            )
            .map_err(failure)
        })
        .collect()
}

#[uniffi::export]
#[must_use]
pub fn protocol_version() -> u32 {
    codex_start_remote::PROTOCOL_VERSION
}
