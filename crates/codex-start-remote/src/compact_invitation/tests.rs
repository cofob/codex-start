use super::*;
use crate::INVITATION_PREFIX;

fn invitation(connection: Connection) -> Invitation {
    let fingerprint = matches!(connection, Connection::Direct { .. })
        .then(|| crypto::digest(b"test certificate"));
    Invitation {
        version: PROTOCOL_VERSION,
        connection,
        connection_password: crypto::random_password(),
        fingerprint,
    }
}

#[test]
fn compact_direct_and_overlay_invitations_round_trip_in_alphanumeric_mode() {
    for connection in [
        Connection::Direct {
            host: "192.168.1.2".into(),
            port: 47321,
        },
        Connection::Direct {
            host: "2001:db8::1".into(),
            port: 443,
        },
        Connection::Yggdrasil {
            public_key: crypto::public_key(&crypto::random_secret()).unwrap(),
            group_password: crypto::random_password(),
            port: 47321,
        },
    ] {
        let original = invitation(connection);
        let uri = original.encode().unwrap();
        assert!(uri.starts_with(INVITATION_PREFIX));
        assert!(
            uri[INVITATION_PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b':')
        );
        assert!(
            uri.len() <= 159,
            "compact QR should not contain JSON or hex keys"
        );
        assert_eq!(Invitation::decode(&uri).unwrap(), original);
        if matches!(original.connection, Connection::Yggdrasil { .. }) {
            assert_eq!(uri.len(), INVITATION_PREFIX.len() + 104);
            assert!(original.fingerprint.is_none());
        }
        assert_eq!(
            crypto::decode_password(&original.connection_password)
                .unwrap()
                .len(),
            16
        );
    }
}

#[test]
fn tls_fingerprint_is_required_only_for_direct_invitations() {
    let mut direct = invitation(Connection::Direct {
        host: "192.168.1.2".into(),
        port: 47321,
    });
    direct.fingerprint = None;
    assert!(direct.encode().is_err());

    let mut overlay = invitation(Connection::Yggdrasil {
        public_key: crypto::public_key(&crypto::random_secret()).unwrap(),
        group_password: crypto::random_password(),
        port: 47321,
    });
    assert!(overlay.encode().is_ok());
    overlay.fingerprint = Some(crypto::digest(b"redundant certificate"));
    assert!(overlay.encode().is_err());
}

#[test]
fn compact_decoder_rejects_old_format_and_malformed_payloads() {
    let original = invitation(Connection::Direct {
        host: "host.example".into(),
        port: 47321,
    });
    let uri = original.encode().unwrap();
    assert!(Invitation::decode("codex-start://connect/e30").is_err());
    assert!(Invitation::decode(&format!("CS:{}", &uri[INVITATION_PREFIX.len()..])).is_err());
    assert!(Invitation::decode(&uri.replace("cs.fob.wtf", "CS.FOB.WTF")).unwrap() == original);
    for prefix in [
        "http://cs.fob.wtf/c#",
        "https://evil.example/c#",
        "https://cs.fob.wtf/C#",
        "https://cs.fob.wtf/c/#",
        "https://CS.FOB.WTF/C/?data=",
    ] {
        assert!(
            Invitation::decode(&format!("{prefix}{}", &uri[INVITATION_PREFIX.len()..])).is_err()
        );
    }
    assert!(Invitation::decode(&uri.to_lowercase()).is_err());
    for end in INVITATION_PREFIX.len()..uri.len() {
        assert!(Invitation::decode(&uri[..end]).is_err());
    }
    let mut bytes = BASE32_NOPAD
        .decode(&uri.as_bytes()[INVITATION_PREFIX.len()..])
        .unwrap();
    bytes.push(0);
    assert!(
        Invitation::decode(&format!(
            "{INVITATION_PREFIX}{}",
            BASE32_NOPAD.encode(&bytes)
        ))
        .is_err()
    );
    bytes.pop();
    bytes[0] = 0xff;
    assert!(
        Invitation::decode(&format!(
            "{INVITATION_PREFIX}{}",
            BASE32_NOPAD.encode(&bytes)
        ))
        .is_err()
    );
}

#[test]
fn passwords_change_length_without_changing_identity_or_device_tokens() {
    let root = tempfile::tempdir().unwrap();
    let mut store = crate::store::Store::open(&root.path().join("state.db")).unwrap();
    let key = store.secret("yggdrasil_key").unwrap();
    let registration = store
        .register(
            crate::Registration {
                name: "phone".into(),
                public_key: crypto::public_key(&crypto::random_secret()).unwrap(),
                connection_password: Some(store.secret("connection_password").unwrap()),
            },
            "fingerprint",
            "nonce",
        )
        .unwrap();
    for name in ["connection_password", "group_password"] {
        store.set_value(name, &crypto::random_secret()).unwrap();
        let password = store.secret(name).unwrap();
        assert_eq!(crypto::decode_password(&password).unwrap().len(), 16);
        assert_eq!(password, store.secret(name).unwrap());
    }
    assert_eq!(key, store.secret("yggdrasil_key").unwrap());
    assert!(store.active(&registration.device_id).unwrap());
    assert_eq!(
        crypto::decode_secret(registration.token.as_ref().unwrap())
            .unwrap()
            .len(),
        32
    );
}
