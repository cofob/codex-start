//! Private daemon identity and durable remote state.

use super::{DaemonOptions, error};
use crate::{
    error::Result,
    paths::{AppPaths, create_private_dir},
};
use codex_start_remote::{
    Connection, DaemonId, Discovery, Invitation, PROTOCOL_VERSION, crypto, store::Store,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, broadcast};

pub struct State {
    pub root: PathBuf,
    pub session_directory: PathBuf,
    pub config: Option<PathBuf>,
    pub files: tokio::sync::Mutex<super::files::Transfers>,
    pub options: DaemonOptions,
    pub discovery: Discovery,
    pub store: Mutex<Store>,
    pub events: broadcast::Sender<u64>,
    pub stop: Notify,
    pub connections: Arc<tokio::sync::Semaphore>,
    pub sessions: tokio::sync::Mutex<super::sessions::Sessions>,
    pub settings_edits: tokio::sync::Mutex<()>,
    pub terminals: tokio::sync::Mutex<super::terminals::Terminals>,
    pub overlay_status: Mutex<String>,
    pub overlay: Mutex<Option<std::sync::Weak<codex_start_transport::overlay::Overlay>>>,
}

pub fn root(config: Option<&Path>) -> Result<PathBuf> {
    let paths = AppPaths::discover()?;
    let suffix = config.map_or_else(
        || "default".to_owned(),
        |p| blake3::hash(p.as_os_str().as_encoded_bytes()).to_hex()[..12].to_owned(),
    );
    let root = paths.data.join("remote").join(suffix);
    create_private_dir(&root)?;
    Ok(root)
}

impl State {
    pub fn open(
        root: PathBuf,
        config: Option<PathBuf>,
        options: DaemonOptions,
        fingerprint: String,
    ) -> Result<Arc<Self>> {
        let store = Store::open(&root.join("state.db")).map_err(error)?;
        let id = if let Some(id) = store.value("daemon_id").map_err(error)? {
            DaemonId(id)
        } else {
            let id = DaemonId::generate();
            store.set_value("daemon_id", &id.0).map_err(error)?;
            id
        };
        for key in ["connection_password", "yggdrasil_key", "group_password"] {
            store.secret(key).map_err(error)?;
        }
        let name = std::process::Command::new("hostname")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "Codex host".into());
        let (events, _) = broadcast::channel(1024);
        Ok(Arc::new(Self {
            session_directory: AppPaths::discover()?.sessions_dir(),
            root,
            files: tokio::sync::Mutex::new(super::files::Transfers::default()),
            config,
            options,
            discovery: Discovery {
                protocol_version: PROTOCOL_VERSION,
                daemon_id: id,
                name,
                fingerprint,
                capabilities: vec![
                    "sessions".into(),
                    "codexRpc".into(),
                    "events".into(),
                    "files".into(),
                    "deviceProof".into(),
                    "projects".into(),
                    "hostPeers".into(),
                    "activeTasks".into(),
                    "launcherSettings".into(),
                    "terminals".into(),
                ],
            },
            store: Mutex::new(store),
            events,
            stop: Notify::new(),
            connections: Arc::new(tokio::sync::Semaphore::new(64)),
            sessions: tokio::sync::Mutex::new(super::sessions::Sessions::default()),
            settings_edits: tokio::sync::Mutex::new(()),
            terminals: tokio::sync::Mutex::new(super::terminals::Terminals::default()),
            overlay_status: Mutex::new("starting".into()),
            overlay: Mutex::new(None),
        }))
    }

    pub fn db<T>(
        &self,
        f: impl FnOnce(&mut Store) -> std::result::Result<T, codex_start_remote::Error>,
    ) -> Result<T> {
        f(&mut *self
            .store
            .lock()
            .map_err(|_| error("remote storage lock poisoned"))?)
        .map_err(error)
    }

    pub fn invitation(&self, host: Option<String>) -> Result<Invitation> {
        let connection = if self.options.no_yggdrasil {
            let host = host
                .or_else(|| self.options.advertise_host.clone())
                .or_else(|| {
                    (!self.options.bind.ip().is_unspecified())
                        .then(|| self.options.bind.ip().to_string())
                })
                .ok_or_else(|| {
                    error("direct QR needs --host HOST or daemon --advertise-host HOST")
                })?;
            Connection::Direct {
                host,
                port: self.options.bind.port(),
            }
        } else {
            Connection::Yggdrasil {
                public_key: self.db(|s| crypto::public_key(&s.secret("yggdrasil_key")?))?,
                group_password: self.db(|s| s.secret("group_password"))?,
                port: self.options.bind.port(),
            }
        };
        Ok(Invitation {
            version: PROTOCOL_VERSION,
            connection,
            connection_password: self.db(|s| s.secret("connection_password"))?,
            fingerprint: self
                .options
                .no_yggdrasil
                .then(|| self.discovery.fingerprint.clone()),
        })
    }

    pub fn publish(
        &self,
        session: &codex_start_remote::SessionId,
        message: &serde_json::Value,
    ) -> Result<()> {
        if let Some(event) = self.db(|s| s.append(session, message))? {
            let _ = self.events.send(event.sequence);
        }
        Ok(())
    }
}
