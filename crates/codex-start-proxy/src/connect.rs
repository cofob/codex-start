//! HTTP CONNECT clients for loopback services and stdin/stdout proxy commands.

use std::{future::Future, io, sync::Arc, time::Duration};

use thiserror::Error;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};
use tracing::warn;

use crate::{
    allowlist::Authority,
    auth::AuthToken,
    relay::{RelayConfig, RelayError, authenticate_client, relay_bidirectional, relay_stdio},
};

const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1_024;
const MIN_PROXY_HEADER_BYTES: usize = 1_024;

/// Configuration for a local listener forwarded through an HTTP CONNECT proxy.
#[derive(Clone, Debug)]
pub struct ConnectBridgeConfig {
    /// Proxy address accepted by `TcpStream::connect`.
    pub proxy: String,
    /// Explicit target authority placed in the CONNECT request.
    pub target: Authority,
    /// Optional sidecar bearer token.
    pub auth_token: Option<AuthToken>,
    /// Resource limits and connect/idle timeouts.
    pub relay: RelayConfig,
}

/// Configuration for a fixed direct TCP forward in bridge/host network modes.
#[derive(Clone, Debug)]
pub struct TcpForwardConfig {
    /// One explicit destination authority.
    pub target: Authority,
    /// Resource limits and connect/idle timeouts.
    pub relay: RelayConfig,
}

/// Configuration for a loopback HTTP proxy that authenticates to the managed
/// egress sidecar without exposing credentials in process arguments or
/// environment variables.
#[derive(Clone, Debug)]
pub struct HttpProxyBridgeConfig {
    /// Managed egress proxy address accepted by [`TcpStream::connect`].
    pub proxy: String,
    /// Bearer token injected into the first request on every connection.
    pub auth_token: AuthToken,
    /// Connection count and connect/header/idle timeouts.
    pub relay: RelayConfig,
    /// Maximum accepted client request-head size.
    pub max_header_bytes: usize,
}

/// Serves a loopback-only forward proxy that adds sidecar authentication.
///
/// The bridge handles exactly one request head per accepted connection. The
/// managed egress sidecar independently validates framing and permits one HTTP
/// transaction (or one CONNECT tunnel), so the bridge is not a policy boundary.
///
/// # Errors
///
/// Returns an error for invalid configuration or a listener-level accept
/// failure. Per-connection failures are isolated and logged.
pub async fn serve_http_proxy_bridge<F>(
    listener: TcpListener,
    config: HttpProxyBridgeConfig,
    shutdown: F,
) -> Result<(), ConnectError>
where
    F: Future<Output = ()>,
{
    validate_http_proxy_bridge(&config)?;
    let config = Arc::new(config);
    let semaphore = Arc::new(Semaphore::new(config.relay.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (local, peer) = accepted.map_err(ConnectError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "HTTP proxy bridge denied: capacity reached");
                    continue;
                };
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_http_proxy_bridge(local, &config).await {
                        warn!(%peer, %error, "HTTP proxy bridge failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "HTTP proxy bridge task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

fn validate_http_proxy_bridge(config: &HttpProxyBridgeConfig) -> Result<(), ConnectError> {
    if config.proxy.is_empty()
        || config.relay.max_connections == 0
        || config.relay.connect_timeout.is_zero()
        || config.relay.handshake_timeout.is_zero()
        || config.relay.idle_timeout.is_zero()
        || config.max_header_bytes < MIN_PROXY_HEADER_BYTES
    {
        return Err(ConnectError::InvalidConfig(
            "proxy address, limits, timeouts, and a header limit of at least 1024 are required"
                .to_owned(),
        ));
    }
    printable_token(&config.auth_token).map(|_| ())
}

async fn handle_http_proxy_bridge(
    mut local: TcpStream,
    config: &HttpProxyBridgeConfig,
) -> Result<(), ConnectError> {
    let (head, remainder) = read_client_request_head(
        &mut local,
        config.max_header_bytes,
        config.relay.handshake_timeout,
    )
    .await?;
    let authenticated = authenticated_request_head(&head, &config.auth_token)?;
    let proxy_name = config.proxy.clone();
    let mut remote = timeout(
        config.relay.connect_timeout,
        TcpStream::connect(&config.proxy),
    )
    .await
    .map_err(|_| ConnectError::ProxyConnectTimeout(proxy_name.clone()))?
    .map_err(|source| ConnectError::ProxyConnect {
        proxy: proxy_name,
        source,
    })?;
    remote.write_all(&authenticated).await?;
    if !remainder.is_empty() {
        remote.write_all(&remainder).await?;
    }
    remote.flush().await?;
    relay_bidirectional(&mut local, &mut remote, config.relay.idle_timeout).await?;
    Ok(())
}

async fn read_client_request_head(
    stream: &mut TcpStream,
    maximum: usize,
    read_timeout: Duration,
) -> Result<(Vec<u8>, Vec<u8>), ConnectError> {
    timeout(read_timeout, async {
        let mut received = Vec::with_capacity(4_096);
        let mut buffer = [0_u8; 4_096];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Err(ConnectError::UnexpectedEof);
            }
            received.extend_from_slice(&buffer[..count]);
            if let Some(end) = find_header_end(&received) {
                if end > maximum {
                    return Err(ConnectError::RequestHeaderTooLarge(maximum));
                }
                return Ok((received[..end].to_vec(), received[end..].to_vec()));
            }
            if received.len() > maximum {
                return Err(ConnectError::RequestHeaderTooLarge(maximum));
            }
        }
    })
    .await
    .map_err(|_| ConnectError::HandshakeTimeout)?
}

fn authenticated_request_head(
    head: &[u8],
    auth_token: &AuthToken,
) -> Result<Vec<u8>, ConnectError> {
    let text = std::str::from_utf8(head).map_err(|_| ConnectError::InvalidProxyRequest)?;
    let lines = text
        .strip_suffix("\r\n\r\n")
        .ok_or(ConnectError::InvalidProxyRequest)?
        .split("\r\n")
        .collect::<Vec<_>>();
    let request_line = lines.first().ok_or(ConnectError::InvalidProxyRequest)?;
    let mut request_parts = request_line.split_ascii_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let target = request_parts.next().unwrap_or_default();
    let version = request_parts.next().unwrap_or_default();
    if method.is_empty()
        || target.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || request_parts.next().is_some()
        || !method.bytes().all(is_http_token_byte)
    {
        return Err(ConnectError::InvalidProxyRequest);
    }

    let token = printable_token(auth_token)?;
    let mut output = Vec::with_capacity(head.len() + token.len() + 32);
    output.extend_from_slice(request_line.as_bytes());
    output.extend_from_slice(b"\r\n");
    for line in lines.iter().skip(1) {
        if line.is_empty()
            || line
                .as_bytes()
                .first()
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
            || line
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(ConnectError::InvalidProxyRequest);
        }
        let (name, _) = line
            .split_once(':')
            .ok_or(ConnectError::InvalidProxyRequest)?;
        if name.is_empty() || name.trim() != name || !name.bytes().all(is_http_token_byte) {
            return Err(ConnectError::InvalidProxyRequest);
        }
        if !name.eq_ignore_ascii_case("proxy-authorization") {
            output.extend_from_slice(line.as_bytes());
            output.extend_from_slice(b"\r\n");
        }
    }
    output.extend_from_slice(b"Proxy-Authorization: Bearer ");
    output.extend_from_slice(token.as_bytes());
    output.extend_from_slice(b"\r\n\r\n");
    Ok(output)
}

fn printable_token(token: &AuthToken) -> Result<&str, ConnectError> {
    std::str::from_utf8(token.expose())
        .ok()
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or(ConnectError::InvalidAuthToken)
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Establishes an HTTP CONNECT tunnel and returns bytes received after its head.
///
/// # Errors
///
/// Returns an error on invalid token bytes, proxy I/O/timeout, oversized or
/// malformed responses, or a non-success response status.
pub async fn establish_connect<S>(
    stream: &mut S,
    target: &Authority,
    auth_token: Option<&AuthToken>,
    handshake_timeout: Duration,
) -> Result<Vec<u8>, ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if handshake_timeout.is_zero() {
        return Err(ConnectError::InvalidConfig(
            "handshake timeout must be non-zero".to_owned(),
        ));
    }
    let mut request =
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: close\r\n");
    if let Some(token) = auth_token {
        let token = std::str::from_utf8(token.expose())
            .ok()
            .filter(|value| value.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or(ConnectError::InvalidAuthToken)?;
        request.push_str("Proxy-Authorization: Bearer ");
        request.push_str(token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");

    timeout(handshake_timeout, async {
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        let mut response = Vec::with_capacity(1_024);
        let mut buffer = [0_u8; 1_024];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Err(ConnectError::UnexpectedEof);
            }
            response.extend_from_slice(&buffer[..count]);
            if let Some(end) = find_header_end(&response) {
                if end > MAX_CONNECT_RESPONSE_BYTES {
                    return Err(ConnectError::ResponseTooLarge);
                }
                validate_response_head(&response[..end])?;
                return Ok(response[end..].to_vec());
            }
            if response.len() > MAX_CONNECT_RESPONSE_BYTES {
                return Err(ConnectError::ResponseTooLarge);
            }
        }
    })
    .await
    .map_err(|_| ConnectError::HandshakeTimeout)?
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn validate_response_head(head: &[u8]) -> Result<(), ConnectError> {
    let text = std::str::from_utf8(head).map_err(|_| ConnectError::InvalidResponse)?;
    let status_line = text
        .split("\r\n")
        .next()
        .ok_or(ConnectError::InvalidResponse)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ConnectError::InvalidResponse);
    }
    let status = status
        .parse::<u16>()
        .map_err(|_| ConnectError::InvalidResponse)?;
    if status != 200 {
        return Err(ConnectError::ProxyDenied(status));
    }
    Ok(())
}

/// Serves a local TCP listener by creating one CONNECT tunnel per client.
///
/// # Errors
///
/// Returns an error for invalid resource limits or a listener-level accept
/// failure. Per-connection failures are isolated and logged.
pub async fn serve_connect_bridge<F>(
    listener: TcpListener,
    config: ConnectBridgeConfig,
    shutdown: F,
) -> Result<(), ConnectError>
where
    F: Future<Output = ()>,
{
    if config.relay.max_connections == 0
        || config.relay.connect_timeout.is_zero()
        || config.relay.handshake_timeout.is_zero()
        || config.relay.idle_timeout.is_zero()
    {
        return Err(ConnectError::InvalidConfig(
            "connection limits and timeouts must be non-zero".to_owned(),
        ));
    }
    let config = Arc::new(config);
    let semaphore = Arc::new(Semaphore::new(config.relay.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (local, peer) = accepted.map_err(ConnectError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "CONNECT bridge denied: capacity reached");
                    continue;
                };
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_bridge(local, &config).await {
                        warn!(%peer, target = %config.target, %error, "CONNECT bridge failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "CONNECT bridge task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

/// Serves a local TCP listener by directly connecting one fixed target.
///
/// This is intended for bridge/host network modes where no egress proxy is
/// present. The caller is responsible for binding the listener to loopback.
///
/// # Errors
///
/// Returns an error for invalid resource limits or a listener-level accept
/// failure. Per-connection failures are isolated and logged.
pub async fn serve_tcp_forward<F>(
    listener: TcpListener,
    config: TcpForwardConfig,
    shutdown: F,
) -> Result<(), ConnectError>
where
    F: Future<Output = ()>,
{
    if config.relay.max_connections == 0
        || config.relay.connect_timeout.is_zero()
        || config.relay.idle_timeout.is_zero()
    {
        return Err(ConnectError::InvalidConfig(
            "connection limits and timeouts must be non-zero".to_owned(),
        ));
    }
    let config = Arc::new(config);
    let semaphore = Arc::new(Semaphore::new(config.relay.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (local, peer) = accepted.map_err(ConnectError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "direct TCP forward denied: capacity reached");
                    continue;
                };
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_tcp_forward(local, &config).await {
                        warn!(%peer, target = %config.target, %error, "direct TCP forward failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "direct TCP forward task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_tcp_forward(
    mut local: TcpStream,
    config: &TcpForwardConfig,
) -> Result<(), ConnectError> {
    let host = config.target.host.lookup_name();
    let mut remote = timeout(
        config.relay.connect_timeout,
        TcpStream::connect((host.as_str(), config.target.port)),
    )
    .await
    .map_err(|_| ConnectError::TargetConnectTimeout(config.target.clone()))?
    .map_err(|source| ConnectError::TargetConnect {
        target: config.target.clone(),
        source,
    })?;
    relay_bidirectional(&mut local, &mut remote, config.relay.idle_timeout).await?;
    Ok(())
}

/// Serves a local Unix socket through HTTP CONNECT and relay authentication.
///
/// This composes the egress sidecar's private-host policy with the
/// authenticated host agent relay, replacing `socat`/`nc` VM fallbacks.
///
/// # Errors
///
/// Returns an error for invalid resource limits or a listener-level accept
/// failure. Per-connection failures are isolated and logged.
#[cfg(unix)]
pub async fn serve_unix_authenticated_connect_bridge<F>(
    listener: UnixListener,
    proxy_address: String,
    target: Authority,
    proxy_token: Option<AuthToken>,
    relay_token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), ConnectError>
where
    F: Future<Output = ()>,
{
    validate_authenticated_bridge_config(&config)?;
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let config = Arc::new(config);
    let proxy_token = Arc::new(proxy_token);
    let relay_token = Arc::new(relay_token);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (local, _) = accepted.map_err(ConnectError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!("authenticated Unix CONNECT bridge denied: capacity reached");
                    continue;
                };
                let proxy_address = proxy_address.clone();
                let target = target.clone();
                let config = Arc::clone(&config);
                let proxy_token = Arc::clone(&proxy_token);
                let relay_token = Arc::clone(&relay_token);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_authenticated_bridge(
                        local,
                        &proxy_address,
                        &target,
                        proxy_token.as_ref().as_ref(),
                        &relay_token,
                        &config,
                    ).await {
                        warn!(%error, %target, "authenticated Unix CONNECT bridge failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "authenticated Unix CONNECT task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

/// Serves a local TCP socket through HTTP CONNECT and relay authentication.
///
/// This is the portable host-loopback bridge: the host relay binds an
/// authenticated random port reachable through the engine gateway, while the
/// workload continues to use a conventional loopback service address.
///
/// # Errors
///
/// Returns an error for invalid resource limits or a listener-level accept
/// failure. Per-connection failures are isolated and logged.
pub async fn serve_tcp_authenticated_connect_bridge<F>(
    listener: TcpListener,
    proxy_address: String,
    target: Authority,
    proxy_token: Option<AuthToken>,
    relay_token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), ConnectError>
where
    F: Future<Output = ()>,
{
    validate_authenticated_bridge_config(&config)?;
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let config = Arc::new(config);
    let proxy_token = Arc::new(proxy_token);
    let relay_token = Arc::new(relay_token);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (local, peer) = accepted.map_err(ConnectError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "authenticated TCP CONNECT bridge denied: capacity reached");
                    continue;
                };
                let proxy_address = proxy_address.clone();
                let target = target.clone();
                let config = Arc::clone(&config);
                let proxy_token = Arc::clone(&proxy_token);
                let relay_token = Arc::clone(&relay_token);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_authenticated_bridge(
                        local,
                        &proxy_address,
                        &target,
                        proxy_token.as_ref().as_ref(),
                        &relay_token,
                        &config,
                    ).await {
                        warn!(%peer, %error, %target, "authenticated TCP CONNECT bridge failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "authenticated TCP CONNECT task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

fn validate_authenticated_bridge_config(config: &RelayConfig) -> Result<(), ConnectError> {
    if config.max_connections == 0
        || config.connect_timeout.is_zero()
        || config.handshake_timeout.is_zero()
        || config.idle_timeout.is_zero()
    {
        Err(ConnectError::InvalidConfig(
            "connection limits and timeouts must be non-zero".to_owned(),
        ))
    } else {
        Ok(())
    }
}

async fn handle_authenticated_bridge<Local>(
    mut local: Local,
    proxy_address: &str,
    target: &Authority,
    proxy_token: Option<&AuthToken>,
    relay_token: &AuthToken,
    config: &RelayConfig,
) -> Result<(), ConnectError>
where
    Local: AsyncRead + AsyncWrite + Unpin,
{
    let mut remote = timeout(config.connect_timeout, TcpStream::connect(proxy_address))
        .await
        .map_err(|_| ConnectError::ProxyConnectTimeout(proxy_address.to_owned()))?
        .map_err(|source| ConnectError::ProxyConnect {
            proxy: proxy_address.to_owned(),
            source,
        })?;
    let remainder =
        establish_connect(&mut remote, target, proxy_token, config.handshake_timeout).await?;
    if !remainder.is_empty() {
        return Err(ConnectError::UnexpectedEarlyData);
    }
    authenticate_client(&mut remote, relay_token, config.handshake_timeout).await?;
    relay_bidirectional(&mut local, &mut remote, config.idle_timeout).await?;
    Ok(())
}

async fn handle_bridge(
    mut local: TcpStream,
    config: &ConnectBridgeConfig,
) -> Result<(), ConnectError> {
    let (mut proxy, remainder) = open_connect_tunnel_with_remainder(
        &config.proxy,
        &config.target,
        config.auth_token.as_ref(),
        &config.relay,
    )
    .await?;
    if !remainder.is_empty() {
        local.write_all(&remainder).await?;
    }
    relay_bidirectional(&mut local, &mut proxy, config.relay.idle_timeout).await?;
    Ok(())
}

/// Opens a checked HTTP CONNECT tunnel suitable for a higher-level protocol.
///
/// # Errors
///
/// Returns an error for proxy connection/handshake failure or if the proxy
/// sends unexpected bytes before the caller starts its protocol.
pub async fn open_connect_tunnel(
    proxy_address: &str,
    target: &Authority,
    auth_token: Option<&AuthToken>,
    config: &RelayConfig,
) -> Result<TcpStream, ConnectError> {
    let (proxy, remainder) =
        open_connect_tunnel_with_remainder(proxy_address, target, auth_token, config).await?;
    if !remainder.is_empty() {
        return Err(ConnectError::UnexpectedEarlyData);
    }
    Ok(proxy)
}

async fn open_connect_tunnel_with_remainder(
    proxy_address: &str,
    target: &Authority,
    auth_token: Option<&AuthToken>,
    config: &RelayConfig,
) -> Result<(TcpStream, Vec<u8>), ConnectError> {
    let mut proxy = timeout(config.connect_timeout, TcpStream::connect(proxy_address))
        .await
        .map_err(|_| ConnectError::ProxyConnectTimeout(proxy_address.to_owned()))?
        .map_err(|source| ConnectError::ProxyConnect {
            proxy: proxy_address.to_owned(),
            source,
        })?;
    let remainder =
        establish_connect(&mut proxy, target, auth_token, config.handshake_timeout).await?;
    Ok((proxy, remainder))
}

/// Connects stdin/stdout to a target through an HTTP CONNECT proxy.
///
/// # Errors
///
/// Returns an error on proxy connection/handshake failure or stream relay
/// timeout/I/O failure.
pub async fn connect_stdio(
    proxy_address: &str,
    target: &Authority,
    auth_token: Option<&AuthToken>,
    config: &RelayConfig,
) -> Result<(), ConnectError> {
    let proxy = timeout(config.connect_timeout, TcpStream::connect(proxy_address))
        .await
        .map_err(|_| ConnectError::ProxyConnectTimeout(proxy_address.to_owned()))?
        .map_err(|source| ConnectError::ProxyConnect {
            proxy: proxy_address.to_owned(),
            source,
        })?;
    connect_streams(
        proxy,
        tokio::io::stdin(),
        tokio::io::stdout(),
        target,
        auth_token,
        config,
    )
    .await
}

/// CONNECTs supplied input/output streams through an existing proxy stream.
///
/// # Errors
///
/// Returns an error on CONNECT handshake or stream relay failure.
pub async fn connect_streams<S, Input, Output>(
    mut proxy: S,
    input: Input,
    mut output: Output,
    target: &Authority,
    auth_token: Option<&AuthToken>,
    config: &RelayConfig,
) -> Result<(), ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
{
    let remainder =
        establish_connect(&mut proxy, target, auth_token, config.handshake_timeout).await?;
    if !remainder.is_empty() {
        output.write_all(&remainder).await?;
        output.flush().await?;
    }
    let (proxy_read, proxy_write) = tokio::io::split(proxy);
    relay_stdio(input, output, proxy_read, proxy_write, config.idle_timeout).await?;
    Ok(())
}

/// HTTP CONNECT bridge failures.
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("invalid CONNECT bridge configuration: {0}")]
    InvalidConfig(String),
    #[error("proxy bearer token is not printable ASCII")]
    InvalidAuthToken,
    #[error("client HTTP proxy request is malformed")]
    InvalidProxyRequest,
    #[error("client HTTP proxy request head exceeded {0} bytes")]
    RequestHeaderTooLarge(usize),
    #[error("CONNECT proxy handshake timed out")]
    HandshakeTimeout,
    #[error("proxy closed before completing CONNECT response")]
    UnexpectedEof,
    #[error("CONNECT proxy response exceeded the size limit")]
    ResponseTooLarge,
    #[error("invalid CONNECT proxy response")]
    InvalidResponse,
    #[error("CONNECT proxy denied the target with HTTP status {0}")]
    ProxyDenied(u16),
    #[error("CONNECT tunnel returned data before the authenticated relay handshake")]
    UnexpectedEarlyData,
    #[error("failed to accept CONNECT bridge connection: {0}")]
    Accept(#[source] io::Error),
    #[error("connection to proxy {0} timed out")]
    ProxyConnectTimeout(String),
    #[error("failed to connect to proxy {proxy}: {source}")]
    ProxyConnect {
        proxy: String,
        #[source]
        source: io::Error,
    },
    #[error("connection to fixed target {0} timed out")]
    TargetConnectTimeout(Authority),
    #[error("failed to connect to fixed target {target}: {source}")]
    TargetConnect {
        target: Authority,
        #[source]
        source: io::Error,
    },
    #[error("CONNECT bridge relay failed: {0}")]
    Relay(#[from] RelayError),
    #[error("CONNECT bridge I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::authenticate_server;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    fn target() -> Authority {
        Authority::parse("host.docker.internal:11434", None).unwrap()
    }

    #[test]
    fn local_proxy_bridge_replaces_client_auth_without_exposing_token_in_config() {
        let token = AuthToken::new(b"per-run-secret".to_vec()).unwrap();
        let request = b"CONNECT api.openai.com:443 HTTP/1.1\r\nHost: api.openai.com:443\r\nProxy-Authorization: Basic attacker\r\n\r\n";
        let authenticated = authenticated_request_head(request, &token).unwrap();
        let authenticated = String::from_utf8(authenticated).unwrap();
        assert!(authenticated.contains("Proxy-Authorization: Bearer per-run-secret\r\n"));
        assert!(!authenticated.contains("Basic attacker"));
        assert_eq!(authenticated.matches("Proxy-Authorization:").count(), 1);
    }

    #[test]
    fn local_proxy_bridge_rejects_ambiguous_request_heads() {
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        for request in [
            b"GET http://example.test/ HTTP/1.1\nHost: example.test\n\n".as_slice(),
            b"GET http://example.test/ HTTP/1.1\r\n folded: yes\r\n\r\n".as_slice(),
            b"GET http://example.test/ HTTP/1.1 extra\r\nHost: example.test\r\n\r\n".as_slice(),
        ] {
            assert!(matches!(
                authenticated_request_head(request, &token),
                Err(ConnectError::InvalidProxyRequest)
            ));
        }
    }

    #[tokio::test]
    async fn local_proxy_bridge_injects_auth_and_relays_response() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.contains("Proxy-Authorization: Bearer bridge-secret\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let bridge = tokio::spawn(async move {
            serve_http_proxy_bridge(
                listener,
                HttpProxyBridgeConfig {
                    proxy: upstream_address.to_string(),
                    auth_token: AuthToken::new(b"bridge-secret".to_vec()).unwrap(),
                    relay: RelayConfig {
                        connect_timeout: Duration::from_secs(1),
                        handshake_timeout: Duration::from_secs(1),
                        idle_timeout: Duration::from_secs(1),
                        ..RelayConfig::default()
                    },
                    max_header_bytes: 4_096,
                },
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });
        let mut client = TcpStream::connect(listener_address).await.unwrap();
        client
            .write_all(b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.ends_with(b"\r\n\r\nok"));
        server.await.unwrap();
        let _ = shutdown_tx.send(());
        bridge.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connect_handshake_formats_target_auth_and_preserves_remainder() {
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        let (mut client, mut server) = duplex(4_096);
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            loop {
                let byte = server.read_u8().await.unwrap();
                request.push(byte);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with(
                "CONNECT host.docker.internal:11434 HTTP/1.1\r\nHost: host.docker.internal:11434\r\n"
            ));
            assert!(request.contains("Proxy-Authorization: Bearer secret\r\n"));
            server
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nearly")
                .await
                .unwrap();
        });
        let remainder =
            establish_connect(&mut client, &target(), Some(&token), Duration::from_secs(1))
                .await
                .unwrap();
        assert_eq!(remainder, b"early");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_streams_relays_both_directions() {
        let (proxy_client, mut proxy_server) = duplex(4_096);
        let (mut input_writer, input_reader) = duplex(128);
        let (output_writer, mut output_reader) = duplex(128);
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(proxy_server.read_u8().await.unwrap());
            }
            proxy_server
                .write_all(b"HTTP/1.1 200 OK\r\n\r\nremote")
                .await
                .unwrap();
            proxy_server.shutdown().await.unwrap();
            let mut input = Vec::new();
            proxy_server.read_to_end(&mut input).await.unwrap();
            assert_eq!(input, b"local");
        });
        input_writer.write_all(b"local").await.unwrap();
        input_writer.shutdown().await.unwrap();
        connect_streams(
            proxy_client,
            input_reader,
            output_writer,
            &target(),
            None,
            &RelayConfig {
                handshake_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(1),
                ..RelayConfig::default()
            },
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"remote");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_rejects_non_success_and_oversized_response() {
        let (mut client, mut server) = duplex(32_768);
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(server.read_u8().await.unwrap());
            }
            server
                .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .await
                .unwrap();
        });
        assert!(matches!(
            establish_connect(&mut client, &target(), None, Duration::from_secs(1)).await,
            Err(ConnectError::ProxyDenied(403))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn direct_tcp_forward_reaches_fixed_target() {
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = target_listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let forward = tokio::spawn(async move {
            serve_tcp_forward(
                listener,
                TcpForwardConfig {
                    target: Authority::parse(&target_address.to_string(), None).unwrap(),
                    relay: RelayConfig::default(),
                },
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });
        let mut client = TcpStream::connect(listen_address).await.unwrap();
        client.write_all(b"fixed target").await.unwrap();
        client.shutdown().await.unwrap();
        let mut echoed = Vec::new();
        client.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"fixed target");
        echo.await.unwrap();
        let _ = shutdown_tx.send(());
        forward.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn authenticated_tcp_bridge_composes_connect_and_relay_authentication() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let relay_token = AuthToken::new(b"relay-secret".to_vec()).unwrap();
        let expected_relay_token = relay_token.clone();
        let proxy_token = AuthToken::new(b"proxy-secret".to_vec()).unwrap();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = proxy_listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("CONNECT host.docker.internal:11434 HTTP/1.1\r\n"));
            assert!(request.contains("Proxy-Authorization: Bearer proxy-secret\r\n"));
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            authenticate_server(&mut stream, &expected_relay_token, Duration::from_secs(1))
                .await
                .unwrap();
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let bridge = tokio::spawn(async move {
            serve_tcp_authenticated_connect_bridge(
                listener,
                proxy_address.to_string(),
                target(),
                Some(proxy_token),
                relay_token,
                RelayConfig {
                    connect_timeout: Duration::from_secs(1),
                    handshake_timeout: Duration::from_secs(1),
                    idle_timeout: Duration::from_secs(1),
                    ..RelayConfig::default()
                },
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });
        let mut client = TcpStream::connect(listen_address).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        proxy.await.unwrap();
        let _ = shutdown_tx.send(());
        bridge.await.unwrap().unwrap();
    }
}
