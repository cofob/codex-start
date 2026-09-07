//! Stable, transport-independent remote API.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47321;
pub const LAST_DISCOVERY_PORT: u16 = 47336;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const CODEX_REVISION: &str = "ad931a45b201e3877d6ba542ba5dbbd85e7e31b4";

macro_rules! identifier {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            #[must_use]
            pub fn generate() -> Self { Self(Uuid::new_v4().to_string()) }
        }
    )+};
}
identifier!(
    DaemonId, DeviceId, ProjectId, SessionId, ThreadId, TurnId, RequestId
);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub protocol_version: u32,
    pub daemon_id: DaemonId,
    pub name: String,
    pub fingerprint: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub name: String,
    pub public_key: String,
    pub connection_password: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationResult {
    pub device_id: DeviceId,
    pub code: Option<String>,
    pub token: Option<String>,
    pub expires_at: Option<u64>,
    pub event_cursor: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authentication {
    pub device_id: DeviceId,
    pub token: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    Authenticate(Authentication),
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Subscribe {
        after: u64,
    },
    Ping,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Challenge {
        nonce: String,
        fingerprint: String,
        discovery: Discovery,
    },
    Authenticated {
        device_id: DeviceId,
    },
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: Option<RequestId>,
        code: String,
        message: String,
    },
    Event(Event),
    Reset {
        oldest: u64,
        latest: u64,
    },
    Pong,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub sequence: u64,
    pub session_id: SessionId,
    pub message: Value,
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub cwd: String,
    pub execution_cwd: String,
    pub environment: String,
    #[serde(default)]
    pub profile: Option<String>,
    pub status: String,
    pub kind: String,
    pub capabilities: Vec<String>,
}

/// A registered app-server is reached through an argv-only local process.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEndpoint {
    pub info: SessionInfo,
    pub program: String,
    pub args: Vec<String>,
    pub owner_pid: Option<u32>,
}

#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
