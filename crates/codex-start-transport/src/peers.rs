//! Bounded public peer downloads with a shared bundled fallback.

use crate::Error;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io::Read, path::Path, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use yggdrasil::core::Core;

const BUNDLED: &str = include_str!("../../../assets/public-peers.json");
const SOURCE: &str = "https://api.github.com/repos/yggdrasil-network/public-peers/tarball/master";
const MAX_DOWNLOAD: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerList {
    pub source: String,
    pub revision: String,
    pub peers: Vec<String>,
}

pub(crate) fn valid(uri: &str) -> bool {
    url::Url::parse(uri).is_ok_and(|u| {
        matches!(u.scheme(), "tcp" | "tls" | "ws" | "wss" | "quic")
            && u.host_str().is_some()
            && u.port_or_known_default().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && uri.len() <= 1024
    })
}

/// Read the peer list included in the build.
///
/// # Panics
/// Panics if the checked-in peer asset is invalid.
#[must_use]
pub fn bundled() -> PeerList {
    serde_json::from_str(BUNDLED).expect("checked bundled peer asset")
}

///
/// # Errors
/// Returns an error if the peer data exceeds a limit or no usable peer is available.
pub fn parse_archive(bytes: &[u8]) -> Result<PeerList, Error> {
    if bytes.len() > MAX_DOWNLOAD {
        return Err(Error::Protocol("peer download is too large".into()));
    }
    // Bound decompression too, including entries that are not Markdown files.
    let decoded = flate2::read::GzDecoder::new(bytes).take((MAX_DOWNLOAD + 1) as u64);
    let mut archive = tar::Archive::new(decoded);
    let mut peers = BTreeSet::new();
    let mut total = 0_usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file()
            || entry.path()?.extension().is_none_or(|e| e != "md")
        {
            continue;
        }
        let size = usize::try_from(entry.size())
            .map_err(|_| Error::Protocol("peer list is too large".into()))?;
        total = total.saturating_add(size);
        if total > MAX_DOWNLOAD {
            return Err(Error::Protocol("peer list is too large".into()));
        }
        let mut content = String::new();
        entry.read_to_string(&mut content)?;
        for uri in content.split('`').skip(1).step_by(2).filter(|v| valid(v)) {
            peers.insert(uri.to_owned());
        }
        if peers.len() > 2000 {
            return Err(Error::Protocol("too many peers".into()));
        }
    }
    if peers.is_empty() {
        return Err(Error::Protocol(
            "peer list contains no supported endpoints".into(),
        ));
    }
    Ok(PeerList {
        source: SOURCE.into(),
        revision: "downloaded".into(),
        peers: peers.into_iter().collect(),
    })
}

pub async fn load(cache: &Path) -> PeerList {
    load_result(cache, download().await).await
}

async fn load_result(cache: &Path, downloaded: Result<PeerList, Error>) -> PeerList {
    if let Ok(list) = downloaded {
        if let Ok(bytes) = serde_json::to_vec(&list) {
            // Saved servers can refresh the common cache at the same time.
            let temp = cache.with_extension(format!(
                "{}.new",
                codex_start_remote::crypto::random_secret()
            ));
            if tokio::fs::write(&temp, bytes).await.is_ok() {
                let _ = tokio::fs::rename(&temp, cache).await;
            }
            let _ = tokio::fs::remove_file(temp).await;
        }
        return list;
    }
    if let Ok(bytes) = read_cache(cache).await
        && bytes.len() <= MAX_DOWNLOAD
        && let Ok(mut list) = serde_json::from_slice::<PeerList>(&bytes)
    {
        list.peers.retain(|uri| valid(uri));
        if !list.peers.is_empty() && list.peers.len() <= 2000 {
            return list;
        }
    }
    bundled()
}

async fn read_cache(cache: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(cache)
        .await?
        .take((MAX_DOWNLOAD + 1) as u64);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn download() -> Result<PeerList, Error> {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(15))
        .user_agent("codex-start/remote-peer-list")
        .build()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    let mut response = client
        .get(SOURCE)
        .send()
        .await
        .map_err(|e| Error::Protocol(e.to_string()))?
        .error_for_status()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Protocol(e.to_string()))?
    {
        if bytes.len() + chunk.len() > MAX_DOWNLOAD {
            return Err(Error::Protocol("peer download is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_archive(&bytes)
}

/// Probe at most twelve peers, four at a time. Measure Yggdrasil link RTT, not ICMP.
///
/// # Errors
/// Returns an error if the peer data exceeds a limit or no usable peer is available.
pub async fn select(core: &Arc<Core>, list: &PeerList) -> Result<Vec<String>, Error> {
    let mut candidates = list.peers.clone();
    candidates.shuffle(&mut rand::thread_rng());
    candidates.truncate(12);
    let mut ranked = Vec::new();
    for batch in candidates.chunks(4) {
        for uri in batch {
            let _ = add_peer(core, uri).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        for peer in core.get_peers().await {
            if peer.up
                && peer.latency_ms.is_finite()
                && peer.latency_ms > 0.0
                && batch.contains(&peer.uri)
            {
                ranked.push((peer.latency_ms, peer.uri));
            }
        }
        for uri in batch {
            let _ = core.remove_peer(uri).await;
        }
    }
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let ranked: Vec<_> = ranked.into_iter().map(|(_, uri)| uri).collect();
    if let Some(primary) = ranked.first() {
        add_peer(core, primary).await?;
    } else {
        return Err(Error::Protocol(
            "no reachable Yggdrasil peer; retry connection".into(),
        ));
    }
    Ok(ranked)
}

// Upstream performs a DNS duplicate check while adding a peer. Bound that check
// too, so an unavailable resolver cannot hold peer selection or failover open.
pub(crate) async fn add_peer(core: &Arc<Core>, uri: &str) -> Result<(), Error> {
    tokio::time::timeout(Duration::from_secs(3), core.add_peer(uri))
        .await
        .map_err(|_| Error::Protocol("peer setup timed out".into()))?
        .map_err(Error::Protocol)
}

/// Put saved host peers first, followed by a random, unique fallback order.
#[must_use]
pub fn bootstrap(preferred: &[String], fallback: &PeerList) -> PeerList {
    let mut shuffled = fallback.peers.clone();
    shuffled.shuffle(&mut rand::thread_rng());
    let mut peers = Vec::new();
    for uri in preferred.iter().take(8).chain(shuffled.iter()) {
        if valid(uri) && !uri.contains('%') && !peers.contains(uri) {
            peers.push(uri.clone());
        }
        if peers.len() == 2000 {
            break;
        }
    }
    PeerList {
        source: fallback.source.clone(),
        revision: fallback.revision.clone(),
        peers,
    }
}

/// Read only locally saved host endpoints. Missing or invalid data is ignored.
pub async fn cached_host(path: &Path) -> Vec<String> {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await
        .is_err()
        || bytes.len() > 16 * 1024
    {
        return Vec::new();
    }
    let peers: Vec<String> = serde_json::from_slice(&bytes).unwrap_or_default();
    host_endpoints(&peers)
}

/// Save authenticated host endpoints without removing a previous cache on failure.
pub async fn remember_host(path: &Path, peers: &[String]) {
    let peers = host_endpoints(peers);
    if peers.is_empty() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(&peers) else {
        return;
    };
    let temporary = path.with_extension(format!(
        "{}.new",
        codex_start_remote::crypto::random_secret()
    ));
    if tokio::fs::write(&temporary, bytes).await.is_ok() {
        let _ = tokio::fs::rename(&temporary, path).await;
    }
    let _ = tokio::fs::remove_file(temporary).await;
}

fn host_endpoints(peers: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for uri in peers.iter().take(8) {
        if valid(uri) && !uri.contains('%') && !result.contains(uri) {
            result.push(uri.clone());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    #[test]
    fn archive_parser_ignores_unsupported_and_non_peer_text() {
        let mut archive = tar::Builder::new(Vec::new());
        let text = b"`tls://peer.example:1234?key=abcd` `wss://peer.example:443` `unix:///tmp/socket` `hello`";
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "peers/europe/test.md", &text[..])
            .unwrap();
        let bytes = archive.into_inner().unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&bytes).unwrap();
        assert_eq!(
            super::parse_archive(&gzip.finish().unwrap()).unwrap().peers,
            vec!["tls://peer.example:1234?key=abcd", "wss://peer.example:443"]
        );
    }

    #[tokio::test]
    async fn failed_download_uses_valid_cache_then_bundled_peers() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("peers.json");
        let unavailable = || Err(crate::Error::Protocol("download unavailable".into()));
        assert_eq!(
            super::load_result(&cache, unavailable()).await.peers,
            super::bundled().peers
        );
        let cached = super::PeerList {
            source: "test".into(),
            revision: "test".into(),
            peers: vec!["tls://peer.example:1234".into()],
        };
        super::load_result(&cache, Ok(cached.clone())).await;
        assert_eq!(
            super::load_result(&cache, unavailable()).await.peers,
            cached.peers
        );
        tokio::fs::write(&cache, b"invalid cache").await.unwrap();
        assert_eq!(
            super::load_result(&cache, unavailable()).await.peers,
            super::bundled().peers
        );
        tokio::fs::write(&cache, vec![b' '; super::MAX_DOWNLOAD + 2])
            .await
            .unwrap();
        assert_eq!(
            super::read_cache(&cache).await.unwrap().len(),
            super::MAX_DOWNLOAD + 1
        );
        assert_eq!(
            super::load_result(&cache, unavailable()).await.peers,
            super::bundled().peers
        );
    }

    #[test]
    fn archive_decompression_is_bounded_for_ignored_files() {
        let mut archive = tar::Builder::new(Vec::new());
        let bytes = vec![b' '; super::MAX_DOWNLOAD + 1024];
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "peers/ignored.txt", &bytes[..])
            .unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&archive.into_inner().unwrap()).unwrap();
        assert!(super::parse_archive(&gzip.finish().unwrap()).is_err());
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn saved_endpoints_are_first_and_fallback_attempts_are_unique() {
        let fallback = PeerList {
            source: "test".into(),
            revision: "1".into(),
            peers: (1..=20)
                .map(|port| format!("tcp://127.0.0.1:{port}"))
                .collect(),
        };
        let preferred = vec![
            fallback.peers[5].clone(),
            fallback.peers[5].clone(),
            "file:///tmp/peer".into(),
        ];
        let result = bootstrap(&preferred, &fallback);
        assert_eq!(result.peers[0], fallback.peers[5]);
        assert_eq!(result.peers.len(), 20);
        assert_eq!(result.peers[..3].iter().collect::<BTreeSet<_>>().len(), 3);
        assert_eq!(
            result.peers.iter().collect::<BTreeSet<_>>(),
            fallback.peers.iter().collect()
        );
    }

    #[tokio::test]
    async fn host_peers_survive_restart_and_empty_updates_keep_previous_addresses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.json");
        assert!(cached_host(&path).await.is_empty());
        let peers = vec![
            "tls://peer.example:443".into(),
            "unix:///tmp/unsafe".into(),
            "tls://peer.example:443".into(),
        ];
        remember_host(&path, &peers).await;
        assert_eq!(cached_host(&path).await, vec![peers[0].clone()]);
        remember_host(&path, &[]).await;
        assert_eq!(cached_host(&path).await, vec![peers[0].clone()]);
        tokio::fs::write(&path, "invalid").await.unwrap();
        assert!(cached_host(&path).await.is_empty());
        tokio::fs::write(&path, vec![b' '; 16 * 1024 + 1])
            .await
            .unwrap();
        assert!(cached_host(&path).await.is_empty());
    }
}
