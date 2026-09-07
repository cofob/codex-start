//! Compact invitation format 1: fixed binary identity fields and unpadded Base32.

use crate::{Connection, DEFAULT_PORT, Error, Invitation, PROTOCOL_VERSION, crypto};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use data_encoding::BASE32_NOPAD;

#[cfg(test)]
mod tests;

fn invalid() -> Error {
    Error::Invalid("invalid compact invitation".into())
}

pub(super) fn encode(invite: &Invitation) -> Result<String, Error> {
    let password = crypto::decode_password(&invite.connection_password)?;
    let mut flags = 0;
    let (port, endpoint) = match &invite.connection {
        Connection::Direct { host, port } => {
            let endpoint = match host.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(ip)) => {
                    flags = 2;
                    ip.octets().to_vec()
                }
                Ok(std::net::IpAddr::V6(ip)) => {
                    flags = 3;
                    ip.octets().to_vec()
                }
                Err(_) => {
                    let mut bytes = vec![u8::try_from(host.len()).map_err(|_| invalid())?];
                    bytes.extend_from_slice(host.as_bytes());
                    bytes
                }
            };
            (*port, endpoint)
        }
        Connection::Yggdrasil {
            public_key,
            group_password,
            port,
        } => {
            let group = crypto::decode_password(group_password)?;
            flags = 1;
            let mut endpoint = crypto::decode32(public_key)?.to_vec();
            endpoint.extend(group);
            (*port, endpoint)
        }
    };
    let mut bytes = vec![0x10 | flags | if port == DEFAULT_PORT { 0 } else { 4 }];
    if port != DEFAULT_PORT {
        bytes.extend(port.to_be_bytes());
    }
    if let Some(fingerprint) = &invite.fingerprint {
        bytes.extend(crypto::decode32(fingerprint)?);
    }
    bytes.extend(password);
    bytes.extend(endpoint);
    Ok(format!(
        "{}{}",
        crate::INVITATION_PREFIX,
        BASE32_NOPAD.encode(&bytes)
    ))
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], Error> {
    if bytes.len() < length {
        return Err(invalid());
    }
    let (value, rest) = bytes.split_at(length);
    *bytes = rest;
    Ok(value)
}

fn text(bytes: &mut &[u8]) -> Result<String, Error> {
    let length = usize::from(take(bytes, 1)?[0]);
    String::from_utf8(take(bytes, length)?.to_vec()).map_err(|_| invalid())
}

pub(super) fn decode(encoded: &str) -> Result<Invitation, Error> {
    let data = BASE32_NOPAD
        .decode(encoded.as_bytes())
        .map_err(|_| invalid())?;
    let mut bytes = data.as_slice();
    let flags = take(&mut bytes, 1)?[0];
    if flags & 0xf8 != 0x10 {
        return Err(invalid());
    }
    let port = if flags & 4 == 0 {
        DEFAULT_PORT
    } else {
        u16::from_be_bytes(take(&mut bytes, 2)?.try_into().map_err(|_| invalid())?)
    };
    let fingerprint = if flags & 3 == 1 {
        None
    } else {
        Some(hex::encode(take(&mut bytes, 32)?))
    };
    let connection_password = URL_SAFE_NO_PAD.encode(take(&mut bytes, 16)?);
    let connection = match flags & 3 {
        0 => Connection::Direct {
            host: text(&mut bytes)?,
            port,
        },
        1 => Connection::Yggdrasil {
            public_key: hex::encode(take(&mut bytes, 32)?),
            group_password: URL_SAFE_NO_PAD.encode(take(&mut bytes, 16)?),
            port,
        },
        2 => Connection::Direct {
            host: std::net::Ipv4Addr::from(
                <[u8; 4]>::try_from(take(&mut bytes, 4)?).map_err(|_| invalid())?,
            )
            .to_string(),
            port,
        },
        _ => Connection::Direct {
            host: std::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(take(&mut bytes, 16)?).map_err(|_| invalid())?,
            )
            .to_string(),
            port,
        },
    };
    if !bytes.is_empty() {
        return Err(invalid());
    }
    Ok(Invitation {
        version: PROTOCOL_VERSION,
        connection,
        connection_password,
        fingerprint,
    })
}
