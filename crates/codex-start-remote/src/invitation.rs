//! Versioned invitations. Debug output intentionally omits their secrets.

use crate::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

pub const INVITATION_PREFIX: &str = "https://cs.fob.wtf/c#";

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Connection {
    Direct {
        host: String,
        port: u16,
    },
    Yggdrasil {
        public_key: String,
        group_password: String,
        port: u16,
    },
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invitation {
    pub version: u32,
    pub connection: Connection,
    pub connection_password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl Invitation {
    ///
    /// # Errors
    /// Returns an error if the invitation has invalid fields or an unsupported version.
    pub fn encode(&self) -> Result<String, crate::Error> {
        self.validate()?;
        super::compact_invitation::encode(self)
    }

    ///
    /// # Errors
    /// Returns an error if the invitation has invalid fields or an unsupported version.
    pub fn decode(uri: &str) -> Result<Self, crate::Error> {
        if uri.len() > 8192 {
            return Err(crate::Error::Invalid("invitation is too large".into()));
        }
        let (_, encoded) = uri
            .split_once("/c#")
            .filter(|(origin, _)| origin.eq_ignore_ascii_case("https://cs.fob.wtf"))
            .ok_or_else(|| crate::Error::Invalid("unsupported invitation".into()))?;
        let result = super::compact_invitation::decode(encoded)?;
        result.validate()?;
        Ok(result)
    }

    ///
    /// # Errors
    /// Returns an error if the invitation has invalid fields or an unsupported version.
    pub fn validate(&self) -> Result<(), crate::Error> {
        if self.version != PROTOCOL_VERSION {
            return Err(crate::Error::Invalid(
                "unsupported invitation version or identity".into(),
            ));
        }
        crate::crypto::decode_password(&self.connection_password)?;
        match &self.connection {
            Connection::Direct { host, port } => {
                crate::crypto::decode32(self.fingerprint.as_deref().ok_or_else(|| {
                    crate::Error::Invalid("direct invitation requires a TLS fingerprint".into())
                })?)?;
                if host.is_empty()
                    || host.len() > 253
                    || host.contains(['/', '?', '#', '@', '\\'])
                    || *port == 0
                {
                    return Err(crate::Error::Invalid("invalid direct endpoint".into()));
                }
            }
            Connection::Yggdrasil {
                public_key,
                group_password,
                port,
            } => {
                if self.fingerprint.is_some() {
                    return Err(crate::Error::Invalid(
                        "Yggdrasil invitations use the host public key, not a TLS fingerprint"
                            .into(),
                    ));
                }
                crate::crypto::decode32(public_key)?;
                crate::crypto::decode_password(group_password)?;
                if *port == 0 {
                    return Err(crate::Error::Invalid("invalid port".into()));
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
