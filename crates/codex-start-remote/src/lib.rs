//! Shared remote protocol, device authentication, and durable event storage.
pub mod crypto;
mod invitation;
mod protocol;
pub mod store;
pub use invitation::{Connection, INVITATION_PREFIX, Invitation};
mod compact_invitation;
pub use protocol::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("device authentication failed")]
    Unauthorized,
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("invalid protocol message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
