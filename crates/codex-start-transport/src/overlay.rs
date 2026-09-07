//! A separate stable Yggdrasil node for each saved server group.

use crate::{Error, Stream, stack::Stack};
use ed25519_dalek::SigningKey;
use std::{
    net::Ipv6Addr,
    path::Path,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};
use tokio::sync::Mutex;
pub use yggdrasil::multicast::NetworkInterface;

pub struct PeerInfo {
    pub public_key: String,
    pub address: String,
    pub active: bool,
    pub inbound: bool,
    pub rtt_ms: f64,
    pub received_bytes: u64,
    pub sent_bytes: u64,
}
use yggdrasil::{config::Config, core::Core, ipv6rwc::ReadWriteCloser};

fn interfaces() -> &'static RwLock<Vec<NetworkInterface>> {
    static INTERFACES: OnceLock<RwLock<Vec<NetworkInterface>>> = OnceLock::new();
    INTERFACES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Supply Android interface names, indices, and IPv6 addresses before connecting.
pub fn set_network_interfaces(mut value: Vec<NetworkInterface>) {
    value.truncate(16);
    if let Ok(mut current) = interfaces().write() {
        *current = value;
    }
}

pub struct Overlay {
    core: Arc<Core>,
    runtime: tokio::runtime::Handle,
    rwc: Arc<ReadWriteCloser>,
    stack: Stack,
    maintenance: tokio::task::JoinHandle<()>,
    ranked: Arc<Mutex<Vec<String>>>,
    multicast: Mutex<bool>,
}

impl Overlay {
    ///
    /// # Errors
    /// Returns an error if the node identity, peer connection, or TCP stack cannot be used.
    pub async fn start(
        secret: &str,
        group_password: &str,
        cache: &Path,
        listen: Option<u16>,
    ) -> Result<Self, Error> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let peers = crate::peers::load(cache).await;
        Self::with_options(secret, group_password, listen, &peers, true, true, false).await
    }

    ///
    /// # Errors
    /// Returns an error if the node identity, peer connection, or TCP stack cannot be used.
    pub async fn with_peers(
        secret: &str,
        group_password: &str,
        listen: Option<u16>,
        peers: &crate::peers::PeerList,
        probe: bool,
    ) -> Result<Self, Error> {
        Self::with_options(secret, group_password, listen, peers, probe, false, false).await
    }

    /// Start client routing without waiting for downloads, probes, or peer DNS.
    ///
    /// # Errors
    /// Returns an error if the node identity cannot be used.
    pub async fn start_client(
        secret: &str,
        group_password: &str,
        preferred: &[String],
    ) -> Result<Self, Error> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let peers = crate::peers::bootstrap(preferred, &crate::peers::bundled());
        Self::with_options(secret, group_password, None, &peers, false, true, true).await
    }

    async fn with_options(
        secret: &str,
        group_password: &str,
        listen: Option<u16>,
        peers: &crate::peers::PeerList,
        probe: bool,
        local_discovery: bool,
        fast: bool,
    ) -> Result<Self, Error> {
        let key = SigningKey::from_bytes(&codex_start_remote::crypto::decode_secret(secret)?);
        let config = Config {
            group_password: group_password.into(),
            listen: Vec::new(),
            peers: Vec::new(),
            node_info_privacy: true,
            ..Config::default()
        };
        let core = Core::new(key, config);
        core.init_links().await;
        let supplied = interfaces()
            .read()
            .map(|value| value.clone())
            .unwrap_or_default();
        let can_discover = !cfg!(target_os = "android") || !supplied.is_empty();
        if !supplied.is_empty() {
            core.update_network_interfaces(supplied);
        }
        let multicast = local_discovery && can_discover && core.start_multicast().await.is_ok();
        let rwc = ReadWriteCloser::new(core.clone(), 1280, None);
        core.set_path_notify(rwc.clone());
        let stack = Stack::new(rwc.clone(), Ipv6Addr::from(core.address().0), listen);
        let ranked = if fast {
            peers.peers.clone()
        } else if probe {
            match crate::peers::select(&core, peers).await {
                Ok(ranked) => ranked,
                Err(error) if !local_discovery => {
                    let _ = core.close().await;
                    return Err(error);
                }
                Err(_) => peers.peers.iter().take(12).cloned().collect(),
            }
        } else {
            for peer in &peers.peers {
                if let Err(error) = crate::peers::add_peer(&core, peer).await {
                    let _ = core.close().await;
                    return Err(error);
                }
            }
            peers.peers.clone()
        };
        let monitor = core.clone();
        let packet_io = rwc.clone();
        let peer_list = peers.clone();
        let ranked = Arc::new(Mutex::new(ranked));
        let alternatives = ranked.clone();
        let maintenance = if fast {
            tokio::spawn(maintain_client(monitor, packet_io, alternatives))
        } else {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                let mut failures = 0;
                loop {
                    interval.tick().await;
                    packet_io.cleanup().await;
                    let selected = alternatives.lock().await.first().cloned();
                    if monitor.get_peers().await.iter().any(|peer| {
                        peer.up && selected.as_ref().is_some_and(|uri| *uri == peer.uri)
                    }) {
                        failures = 0;
                        continue;
                    }
                    failures += 1;
                    monitor.retry_peers_now().await;
                    if failures >= 2 {
                        let mut alternatives = alternatives.lock().await;
                        if let Some(old) = alternatives.first() {
                            let _ = monitor.remove_peer(old).await;
                        }
                        if !alternatives.is_empty() {
                            alternatives.remove(0);
                        }
                        if let Some(next) = alternatives.first() {
                            let _ = crate::peers::add_peer(&monitor, next).await;
                        } else if let Ok(selected) =
                            crate::peers::select(&monitor, &peer_list).await
                        {
                            *alternatives = selected;
                        }
                        failures = 0;
                    }
                }
            })
        };
        Ok(Self {
            runtime: tokio::runtime::Handle::current(),
            core,
            rwc,
            stack,
            maintenance,
            ranked,
            multicast: Mutex::new(multicast),
        })
    }

    ///
    /// # Errors
    /// Returns an error if the node identity, peer connection, or TCP stack cannot be used.
    pub async fn connect(&self, public_key: &str, port: u16) -> Result<Stream, Error> {
        let key = codex_start_remote::crypto::decode32(public_key)?;
        self.rwc.update_key(key).await;
        self.stack
            .connect(
                Ipv6Addr::from(yggdrasil::address::addr_for_key(&key).0),
                port,
            )
            .await
    }
    pub async fn accept(&self) -> Option<Stream> {
        self.stack.accept().await
    }
    pub fn address(&self) -> Ipv6Addr {
        Ipv6Addr::from(self.core.address().0)
    }
    pub async fn network_changed(&self) {
        let supplied = interfaces()
            .read()
            .map(|value| value.clone())
            .unwrap_or_default();
        let available = !supplied.is_empty();
        if cfg!(target_os = "android") {
            self.core.update_network_interfaces(supplied);
        }
        let mut multicast = self.multicast.lock().await;
        if !*multicast && available {
            *multicast = self.core.start_multicast().await.is_ok();
        }
        drop(multicast);
        self.core.retry_peers_now().await;
        self.core.force_router_refresh();
    }

    pub async fn is_connected(&self) -> bool {
        self.core.get_peers().await.iter().any(|peer| peer.up)
    }

    pub async fn peer_info(&self) -> Vec<PeerInfo> {
        self.core
            .get_peers()
            .await
            .into_iter()
            .take(32)
            .map(|peer| PeerInfo {
                public_key: hex::encode(peer.key),
                address: peer.uri,
                active: peer.up,
                inbound: peer.inbound,
                rtt_ms: peer.latency_ms,
                received_bytes: peer.rx_bytes as u64,
                sent_bytes: peer.tx_bytes as u64,
            })
            .collect()
    }

    /// Public outbound peers only; local multicast links are discovered on each device.
    pub async fn active_peers(&self) -> Vec<String> {
        self.core
            .get_peers()
            .await
            .into_iter()
            .filter(|peer| {
                peer.up
                    && !peer.inbound
                    && crate::peers::valid(&peer.uri)
                    && !peer.uri.contains('%')
            })
            .map(|peer| peer.uri)
            .take(8)
            .collect()
    }

    /// Try the authenticated host's peers while keeping the working route available.
    pub async fn prefer_peers(&self, peers: &[String]) {
        let candidates: Vec<_> = peers
            .iter()
            .filter(|uri| crate::peers::valid(uri) && !uri.contains('%'))
            .take(4)
            .cloned()
            .collect();
        if candidates.is_empty() {
            return;
        }
        let mut ranked = self.ranked.lock().await;
        for peer in &candidates {
            let _ = crate::peers::add_peer(&self.core, peer).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut reachable: Vec<_> = self
            .core
            .get_peers()
            .await
            .into_iter()
            .filter(|peer| peer.up && candidates.contains(&peer.uri))
            .collect();
        reachable.sort_by(|a, b| a.latency_ms.total_cmp(&b.latency_ms));
        let Some(preferred) = reachable.first().map(|peer| peer.uri.clone()) else {
            for peer in &candidates {
                if !ranked.contains(peer) {
                    let _ = self.core.remove_peer(peer).await;
                }
            }
            return;
        };
        for peer in ranked
            .iter()
            .chain(candidates.iter())
            .filter(|peer| **peer != preferred)
        {
            let _ = self.core.remove_peer(peer).await;
        }
        let mut alternatives = vec![preferred];
        for peer in candidates.into_iter().chain(ranked.iter().cloned()) {
            if !alternatives.contains(&peer) {
                alternatives.push(peer);
            }
        }
        *ranked = alternatives;
        self.core.force_router_refresh();
    }
}

/// Keep three outbound attempts. Rotate failed slots after a ten-second grace
/// period; a working public or LAN link must not hide a failed slot.
async fn maintain_client(
    core: Arc<Core>,
    rwc: Arc<ReadWriteCloser>,
    ranked: Arc<Mutex<Vec<String>>>,
) {
    let mut failures = std::collections::BTreeMap::<String, u8>::new();
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        rwc.cleanup().await;
        let current = core.get_peers().await;
        let mut order = ranked.lock().await;
        let selected: Vec<_> = order.iter().take(3).cloned().collect();
        failures.retain(|uri, _| selected.contains(uri));
        for uri in &selected {
            let up = current.iter().any(|peer| peer.up && peer.uri == *uri);
            if up {
                failures.remove(uri);
                continue;
            }
            let count = failures.entry(uri.clone()).or_default();
            *count += 1;
            if *count >= 3 {
                let _ = core.remove_peer(uri).await;
                if let Some(index) = order.iter().position(|peer| peer == uri) {
                    let old = order.remove(index);
                    order.push(old);
                }
                failures.remove(uri);
            }
        }
        let candidates: Vec<_> = order
            .iter()
            .take(3)
            .filter(|uri| !current.iter().any(|peer| peer.uri == **uri && peer.up))
            .cloned()
            .collect();
        // AddPeer includes a DNS check. Run the three checks in parallel and
        // keep ownership under this task so dropping the overlay cancels them.
        futures::future::join_all(
            candidates
                .iter()
                .map(|uri| crate::peers::add_peer(&core, uri)),
        )
        .await;
        core.retry_peers_now().await;
        core.force_router_refresh();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.maintenance.abort();
        let core = self.core.clone();
        self.runtime.spawn(async move {
            core.close_multicast().await;
            let _ = core.close().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_start_remote::crypto;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn relay() -> (Arc<Core>, crate::peers::PeerList) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("tcp://{}", listener.local_addr().unwrap());
        drop(listener);
        let key = SigningKey::from_bytes(&crypto::decode_secret(&crypto::random_secret()).unwrap());
        let core = Core::new(
            key,
            Config {
                multicast_interfaces: Vec::new(),
                ..Config::default()
            },
        );
        core.init_links().await;
        core.listen(&address).await.unwrap();
        (
            core,
            crate::peers::PeerList {
                source: "local test relay".into(),
                revision: "test".into(),
                peers: vec![address],
            },
        )
    }

    #[tokio::test]
    async fn separate_endpoint_groups_share_a_public_relay_without_link_passwords() {
        let (relay, peers) = relay().await;
        let group_a = crypto::random_secret();
        let group_b = crypto::random_secret();
        let key_a = crypto::random_secret();
        let key_b = crypto::random_secret();
        let host_a = Overlay::with_peers(&key_a, &group_a, Some(47321), &peers, false)
            .await
            .unwrap();
        let host_b = Overlay::with_peers(&key_b, &group_b, Some(47321), &peers, false)
            .await
            .unwrap();
        let phone_a = Overlay::with_peers(&crypto::random_secret(), &group_a, None, &peers, false)
            .await
            .unwrap();
        let phone_b = Overlay::with_peers(&crypto::random_secret(), &group_b, None, &peers, false)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            for (host, phone, key) in [(&host_a, &phone_a, &key_a), (&host_b, &phone_b, &key_b)] {
                let mut client = phone
                    .connect(&crypto::public_key(key).unwrap(), 47321)
                    .await
                    .unwrap();
                client
                    .write_all(b"same protocol, separate group")
                    .await
                    .unwrap();
                let mut server = host.accept().await.unwrap();
                let mut data = [0; 29];
                server.read_exact(&mut data).await.unwrap();
                assert_eq!(&data, b"same protocol, separate group");
                server.write_all(b"ok").await.unwrap();
                let mut reply = [0; 2];
                client.read_exact(&mut reply).await.unwrap();
                assert_eq!(&reply, b"ok");
            }
        })
        .await
        .expect("userspace TCP must connect through the public relay");
        let mut wrong = phone_b
            .connect(&crypto::public_key(&key_a).unwrap(), 47321)
            .await
            .unwrap();
        wrong.write_all(b"blocked").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), host_a.accept())
                .await
                .is_err()
        );
        drop((host_a, host_b, phone_a, phone_b));
        relay.close().await.unwrap();
    }

    #[tokio::test]
    async fn stable_identity_reconnects_while_old_remote_sockets_remain_open() {
        let (relay, peers) = relay().await;
        let group = crypto::random_password();
        let host_key = crypto::random_secret();
        let phone_key = crypto::random_secret();
        let host = Overlay::with_peers(&host_key, &group, Some(47321), &peers, false)
            .await
            .unwrap();
        let mut old_sockets = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            for attempt in 0..3_u8 {
                let phone = Overlay::with_peers(&phone_key, &group, None, &peers, false)
                    .await
                    .unwrap();
                let mut stream = phone
                    .connect(&crypto::public_key(&host_key).unwrap(), 47321)
                    .await
                    .unwrap();
                stream.write_all(&[attempt]).await.unwrap();
                let mut remote = host.accept().await.unwrap();
                assert_eq!(remote.read_u8().await.unwrap(), attempt);
                remote.write_all(b"ok").await.unwrap();
                let mut reply = [0; 2];
                stream.read_exact(&mut reply).await.unwrap();
                assert_eq!(&reply, b"ok");
                old_sockets.push(remote);
                // Abort the stack as on Android process death; do not send a FIN.
                drop(phone);
                drop(stream);
            }
        })
        .await
        .expect("same-key reconnect must not wait for old TCP connections to expire");
        drop((host, old_sockets));
        relay.close().await.unwrap();
    }

    #[tokio::test]
    async fn established_stream_recovers_when_the_selected_peer_stops() {
        let (relay_a, peers_a) = relay().await;
        let (relay_b, peers_b) = relay().await;
        relay_b.add_peer(&peers_a.peers[0]).await.unwrap();
        let peers = crate::peers::PeerList {
            source: "local failover relays".into(),
            revision: "test".into(),
            peers: vec![peers_a.peers[0].clone(), peers_b.peers[0].clone()],
        };
        let group = crypto::random_password();
        let host_key = crypto::random_secret();
        let host = Overlay::with_peers(&host_key, &group, Some(47321), &peers, true)
            .await
            .unwrap();
        let phone = Overlay::with_peers(&crypto::random_secret(), &group, None, &peers, true)
            .await
            .unwrap();
        let mut client = phone
            .connect(&crypto::public_key(&host_key).unwrap(), 47321)
            .await
            .unwrap();
        let mut remote = tokio::time::timeout(Duration::from_secs(20), async {
            client.write_all(b"before").await.unwrap();
            let mut remote = host.accept().await.unwrap();
            let mut bytes = [0; 6];
            remote.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"before");
            remote
        })
        .await
        .unwrap();
        let selected = host
            .core
            .get_peers()
            .await
            .into_iter()
            .find(|peer| peer.up)
            .unwrap()
            .uri;
        let remaining = if selected == peers_a.peers[0] {
            relay_a.close().await.unwrap();
            relay_b
        } else {
            assert_eq!(selected, peers_b.peers[0]);
            relay_b.close().await.unwrap();
            relay_a
        };
        // Use the production maintenance interval and the existing TCP stream.
        // The alternate must recover before the 120-second TCP timeout expires.
        tokio::time::timeout(Duration::from_secs(100), async {
            client.write_all(b"after").await.unwrap();
            let mut bytes = [0; 5];
            remote.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"after");
            remote.write_all(b"ok").await.unwrap();
            let mut reply = [0; 2];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"ok");
        })
        .await
        .expect("selected-peer loss must not require a new TCP connection");
        drop((host, phone, client, remote));
        remaining.close().await.unwrap();
    }

    #[tokio::test]
    async fn public_peer_is_replaced_while_an_inbound_lan_link_stays_up() {
        let (relay_a, peers_a) = relay().await;
        let (relay_b, peers_b) = relay().await;
        let (lan, _) = relay().await;
        let mut peers = peers_a.clone();
        peers.peers.extend(peers_b.peers.clone());
        let host = Overlay::with_peers(
            &crypto::random_secret(),
            &crypto::random_password(),
            None,
            &peers,
            true,
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("tcp://{}", listener.local_addr().unwrap());
        drop(listener);
        host.core.listen(&address).await.unwrap();
        lan.add_peer(&address).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !host
                .core
                .get_peers()
                .await
                .iter()
                .any(|peer| peer.up && peer.inbound)
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let selected = host.ranked.lock().await[0].clone();
        let remaining = if selected == peers_a.peers[0] {
            relay_a.close().await.unwrap();
            relay_b
        } else {
            relay_b.close().await.unwrap();
            relay_a
        };
        tokio::time::timeout(Duration::from_secs(100), async {
            loop {
                let current = host.ranked.lock().await[0].clone();
                if current != selected
                    && host
                        .core
                        .get_peers()
                        .await
                        .iter()
                        .any(|peer| peer.up && peer.uri == current)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("a LAN link must not mask failure of the selected public peer");
        assert!(
            host.core
                .get_peers()
                .await
                .iter()
                .any(|peer| peer.up && peer.inbound)
        );
        drop(host);
        remaining.close().await.unwrap();
        lan.close().await.unwrap();
    }

    #[tokio::test]
    async fn host_peer_preference_keeps_the_stream_and_rejects_unreachable_replacements() {
        let (relay_a, peers_a) = relay().await;
        let (relay_b, peers_b) = relay().await;
        relay_b.add_peer(&peers_a.peers[0]).await.unwrap();
        let group = crypto::random_password();
        let key = crypto::random_secret();
        let host = Overlay::with_peers(&key, &group, Some(47321), &peers_b, false)
            .await
            .unwrap();
        let phone = Overlay::with_peers(&crypto::random_secret(), &group, None, &peers_a, false)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut stream = phone
                .connect(&crypto::public_key(&key).unwrap(), 47321)
                .await
                .unwrap();
            stream.write_all(b"a").await.unwrap();
            let mut remote = host.accept().await.unwrap();
            assert_eq!(remote.read_u8().await.unwrap(), b'a');
            phone.prefer_peers(&host.active_peers().await).await;
            assert_eq!(phone.active_peers().await, peers_b.peers);
            stream.write_all(b"b").await.unwrap();
            assert_eq!(remote.read_u8().await.unwrap(), b'b');
            let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let unavailable = format!("tcp://{}", unused.local_addr().unwrap());
            drop(unused);
            phone.prefer_peers(&[unavailable]).await;
            assert_eq!(phone.active_peers().await, peers_b.peers);
            remote.write_all(b"c").await.unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), b'c');
        })
        .await
        .expect("host peer preference must preserve the active stream");
        drop((host, phone));
        relay_a.close().await.unwrap();
        relay_b.close().await.unwrap();
    }
    #[tokio::test]
    async fn fast_start_keeps_three_attempts_and_replaces_a_failed_peer() {
        let mut relays = Vec::new();
        let mut addresses = Vec::new();
        for _ in 0..4 {
            let (core, peers) = relay().await;
            relays.push(core);
            addresses.push(peers.peers[0].clone());
        }
        let peers = crate::peers::PeerList {
            source: "test".into(),
            revision: "1".into(),
            peers: addresses.clone(),
        };
        let phone = tokio::time::timeout(
            Duration::from_secs(1),
            Overlay::with_options(
                &crypto::random_secret(),
                &crypto::random_secret(),
                None,
                &peers,
                false,
                false,
                true,
            ),
        )
        .await
        .expect("startup must not wait for peer probes")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let active = phone.active_peers().await;
                if addresses[..3].iter().all(|uri| active.contains(uri)) {
                    assert!(!active.contains(&addresses[3]));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("three initial peers must connect together");
        relays[0].close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let active = phone.active_peers().await;
                assert!(
                    active.contains(&addresses[1]) && active.contains(&addresses[2]),
                    "working peers must remain connected"
                );
                if active.contains(&addresses[3]) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the fourth peer must replace the failed first peer");
        drop(phone);
        for relay in &relays[1..] {
            relay.close().await.unwrap();
        }
    }
}
