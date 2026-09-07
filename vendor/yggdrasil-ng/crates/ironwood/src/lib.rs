pub mod config;
pub mod core;
pub mod types;

pub(crate) mod bloom;
pub(crate) mod crypto;
pub mod encrypted;
pub(crate) mod pathfinder;
pub(crate) mod peers;
pub(crate) mod router;
pub mod signed;
pub(crate) mod traffic;
pub(crate) mod wire;

// Re-export primary public API
pub use crate::config::Config;
pub use crate::core::{
    DebugSnapshot, PacketConnImpl, PathEntry, PeerInfo, TreeEntry, new_packet_conn,
};
pub use crate::encrypted::{EncryptedPacketConn, SessionEntry, new_encrypted_packet_conn};
pub use crate::signed::{SignedPacketConn, new_signed_packet_conn};
pub use crate::types::{Addr, Error, PacketConn, Result};
