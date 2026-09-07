//! Native client used by Android and protocol integration tests.
mod client;
mod files;
pub use client::{Client, SavedServer, discover};
pub use codex_start_transport::overlay::{NetworkInterface, PeerInfo, set_network_interfaces};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Remote(#[from] codex_start_remote::Error),
    #[error("{0}")]
    Transport(#[from] codex_start_transport::Error),
    #[error("{0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}
