//! Authenticated browser opening and reverse OAuth callback tunnels.

use std::{
    ffi::OsString, future::Future, io, path::PathBuf, process::Stdio, sync::Arc, time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};
use tracing::{info, warn};
use url::Url;

use crate::{
    allowlist::{AllowList, AllowlistError, Authority, NormalizedHost},
    auth::AuthToken,
    relay::{
        RelayConfig, RelayError, RelayTarget, authenticate_client, authenticate_server,
        serve_authenticated_tcp, serve_tcp_bridge,
    },
};

const MAX_OPEN_REQUEST_BYTES: usize = 32 * 1_024;
const MAX_RESPONSE_LINE_BYTES: usize = 1_024;

/// Host-side browser opener configuration.
#[derive(Clone, Debug)]
pub struct BrowserOpenConfig {
    /// URL authorities the container may ask the host to open.
    pub allowlist: AllowList,
    /// Host browser opener executable (`open`, `xdg-open`, or equivalent).
    pub opener_program: PathBuf,
    /// Fixed trusted arguments inserted before the URL.
    pub opener_args: Vec<OsString>,
    /// Authentication, concurrency, and request timeout values.
    pub relay: RelayConfig,
    /// Maximum time for the opener process to return.
    pub opener_timeout: Duration,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowserOpenRequest {
    url: String,
}

/// Parses a safe browser URL and returns its normalized authority.
///
/// # Errors
///
/// Returns an error unless the URL uses HTTP(S), contains a host and known or
/// explicit port, and contains no URL credentials.
pub fn validate_browser_url(input: &str) -> Result<(Url, Authority), BrowserError> {
    if input.len() > MAX_OPEN_REQUEST_BYTES || input.contains(['\0', '\r', '\n']) {
        return Err(BrowserError::InvalidUrl);
    }
    let authority_text = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .filter(|authority| !authority.is_empty());
    if authority_text.is_none() {
        return Err(BrowserError::InvalidUrl);
    }
    let url = Url::parse(input).map_err(|_| BrowserError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(BrowserError::InvalidUrl);
    }
    let host = url.host_str().ok_or(BrowserError::InvalidUrl)?;
    let port = url
        .port_or_known_default()
        .ok_or(BrowserError::InvalidUrl)?;
    let authority = Authority {
        host: NormalizedHost::parse(host)?,
        port,
    };
    Ok((url, authority))
}

/// Serves authenticated browser-open requests until `shutdown` resolves.
///
/// # Errors
///
/// Returns an error for invalid limits or a listener-level accept failure.
/// Request denials and opener failures are isolated, logged without URL query
/// data, and reported to the requesting client.
pub async fn serve_browser_open<F>(
    listener: TcpListener,
    token: AuthToken,
    config: BrowserOpenConfig,
    shutdown: F,
) -> Result<(), BrowserError>
where
    F: Future<Output = ()>,
{
    if config.allowlist.is_empty()
        || config.relay.max_connections == 0
        || config.relay.handshake_timeout.is_zero()
        || config.opener_timeout.is_zero()
    {
        return Err(BrowserError::InvalidConfig);
    }
    let config = Arc::new(config);
    let token = Arc::new(token);
    let semaphore = Arc::new(Semaphore::new(config.relay.max_connections));
    let mut requests = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer) = accepted.map_err(BrowserError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "browser-open request denied: capacity reached");
                    continue;
                };
                let token = Arc::clone(&token);
                let config = Arc::clone(&config);
                requests.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_browser_open(stream, &token, &config).await {
                        warn!(%peer, %error, "browser-open request failed");
                    }
                });
            }
            completed = requests.join_next(), if !requests.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "browser-open task panicked or was cancelled");
                }
            }
        }
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    Ok(())
}

async fn handle_browser_open(
    mut stream: TcpStream,
    token: &AuthToken,
    config: &BrowserOpenConfig,
) -> Result<(), BrowserError> {
    authenticate_server(&mut stream, token, config.relay.handshake_timeout).await?;
    let request = match read_open_request(&mut stream, config.relay.handshake_timeout).await {
        Ok(request) => request,
        Err(error) => {
            send_open_response(&mut stream, false).await;
            return Err(error);
        }
    };
    let (url, authority) = match validate_browser_url(&request.url) {
        Ok((url, authority)) if config.allowlist.allows(&authority) => (url, authority),
        Ok((_, authority)) => {
            send_open_response(&mut stream, false).await;
            return Err(BrowserError::UrlDenied(authority));
        }
        Err(error) => {
            send_open_response(&mut stream, false).await;
            return Err(error);
        }
    };

    let status = timeout(
        config.opener_timeout,
        Command::new(&config.opener_program)
            .args(&config.opener_args)
            .arg(url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| BrowserError::OpenerTimeout)?
    .map_err(|source| BrowserError::SpawnOpener {
        program: config.opener_program.clone(),
        source,
    })?;
    if !status.success() {
        send_open_response(&mut stream, false).await;
        return Err(BrowserError::OpenerFailed(status));
    }
    send_open_response(&mut stream, true).await;
    info!(target = %authority, "browser-open request allowed");
    Ok(())
}

async fn read_open_request(
    stream: &mut TcpStream,
    request_timeout: Duration,
) -> Result<BrowserOpenRequest, BrowserError> {
    timeout(request_timeout, async {
        let length =
            usize::try_from(stream.read_u32().await?).map_err(|_| BrowserError::RequestTooLarge)?;
        if length == 0 || length > MAX_OPEN_REQUEST_BYTES {
            return Err(BrowserError::RequestTooLarge);
        }
        let mut request = vec![0_u8; length];
        stream.read_exact(&mut request).await?;
        serde_json::from_slice(&request).map_err(BrowserError::InvalidRequest)
    })
    .await
    .map_err(|_| BrowserError::RequestTimeout)?
}

async fn send_open_response(stream: &mut TcpStream, allowed: bool) {
    let response: &[u8] = if allowed { b"OK\n" } else { b"ERR denied\n" };
    let _ = stream.write_all(response).await;
    let _ = stream.shutdown().await;
}

/// Requests that an authenticated host endpoint open one URL.
///
/// # Errors
///
/// Returns an error for an invalid URL, connection/authentication failure,
/// bounded protocol failure, or host denial.
pub async fn request_browser_open(
    remote: &str,
    token: &AuthToken,
    url: &str,
    config: &RelayConfig,
) -> Result<(), BrowserError> {
    validate_browser_url(url)?;
    let stream = timeout(config.connect_timeout, TcpStream::connect(remote))
        .await
        .map_err(|_| BrowserError::ConnectTimeout(remote.to_owned()))?
        .map_err(|source| BrowserError::Connect {
            remote: remote.to_owned(),
            source,
        })?;
    request_browser_open_stream(stream, token, url, config.handshake_timeout).await
}

/// Sends a browser-open request over a supplied authenticated stream.
///
/// # Errors
///
/// Returns an error for invalid input, authentication, protocol I/O, timeout,
/// or host denial.
pub async fn request_browser_open_stream<S>(
    mut stream: S,
    token: &AuthToken,
    url: &str,
    request_timeout: Duration,
) -> Result<(), BrowserError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    validate_browser_url(url)?;
    authenticate_client(&mut stream, token, request_timeout).await?;
    let request = serde_json::to_vec(&BrowserOpenRequest {
        url: url.to_owned(),
    })
    .map_err(BrowserError::EncodeRequest)?;
    if request.len() > MAX_OPEN_REQUEST_BYTES {
        return Err(BrowserError::RequestTooLarge);
    }
    stream
        .write_u32(u32::try_from(request.len()).map_err(|_| BrowserError::RequestTooLarge)?)
        .await?;
    stream.write_all(&request).await?;
    stream.flush().await?;
    let response = read_bounded_line(&mut stream, request_timeout).await?;
    if response == b"OK" {
        Ok(())
    } else {
        Err(BrowserError::ServerDenied)
    }
}

async fn read_bounded_line<S>(
    stream: &mut S,
    response_timeout: Duration,
) -> Result<Vec<u8>, BrowserError>
where
    S: AsyncRead + Unpin,
{
    timeout(response_timeout, async {
        let mut line = Vec::new();
        loop {
            let byte = stream.read_u8().await?;
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if line.len() >= MAX_RESPONSE_LINE_BYTES {
                return Err(BrowserError::InvalidResponse);
            }
            line.push(byte);
        }
    })
    .await
    .map_err(|_| BrowserError::RequestTimeout)?
}

/// Runs the host half of a reverse OAuth callback tunnel.
///
/// The supplied listener must be bound to loopback. Each browser callback is
/// forwarded to an authenticated container target at `remote`.
///
/// # Errors
///
/// Returns an error for a non-loopback listener, invalid limits, or a
/// listener-level relay failure.
pub async fn serve_oauth_host_listener<F>(
    listener: TcpListener,
    remote: String,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), OAuthTunnelError>
where
    F: Future<Output = ()>,
{
    let address = listener
        .local_addr()
        .map_err(OAuthTunnelError::ListenerAddress)?;
    require_loopback(address)?;
    serve_tcp_bridge(listener, remote, token, config, shutdown)
        .await
        .map_err(OAuthTunnelError::Relay)
}

/// Runs the container half of a reverse OAuth callback tunnel.
///
/// Authenticated host streams are connected only to the explicit loopback
/// `callback` target.
///
/// # Errors
///
/// Returns an error for a non-loopback callback, invalid limits, or a
/// listener-level relay failure.
pub async fn serve_oauth_callback_target<F>(
    listener: TcpListener,
    callback: std::net::SocketAddr,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), OAuthTunnelError>
where
    F: Future<Output = ()>,
{
    require_loopback(callback)?;
    serve_authenticated_tcp(
        listener,
        RelayTarget::Tcp(callback.to_string()),
        token,
        config,
        shutdown,
    )
    .await
    .map_err(OAuthTunnelError::Relay)
}

fn require_loopback(address: std::net::SocketAddr) -> Result<(), OAuthTunnelError> {
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(OAuthTunnelError::NonLoopback(address));
    }
    Ok(())
}

/// Browser opener protocol errors.
#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("invalid browser-open configuration")]
    InvalidConfig,
    #[error("invalid browser URL")]
    InvalidUrl,
    #[error("browser URL authority denied: {0}")]
    UrlDenied(Authority),
    #[error("browser-open request is too large")]
    RequestTooLarge,
    #[error("browser-open request timed out")]
    RequestTimeout,
    #[error("invalid browser-open request: {0}")]
    InvalidRequest(#[source] serde_json::Error),
    #[error("failed to encode browser-open request: {0}")]
    EncodeRequest(#[source] serde_json::Error),
    #[error("invalid browser-open response")]
    InvalidResponse,
    #[error("host denied browser-open request")]
    ServerDenied,
    #[error("browser opener timed out")]
    OpenerTimeout,
    #[error("failed to start browser opener {program}: {source}")]
    SpawnOpener {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("browser opener exited with {0}")]
    OpenerFailed(std::process::ExitStatus),
    #[error("failed to accept browser-open connection: {0}")]
    Accept(#[source] io::Error),
    #[error("connection to browser-open bridge {0} timed out")]
    ConnectTimeout(String),
    #[error("failed to connect to browser-open bridge {remote}: {source}")]
    Connect {
        remote: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid browser URL authority: {0}")]
    Allowlist(#[from] AllowlistError),
    #[error("browser-open authentication failed: {0}")]
    Authentication(#[from] RelayError),
    #[error("browser-open I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Reverse OAuth callback tunnel errors.
#[derive(Debug, Error)]
pub enum OAuthTunnelError {
    #[error("OAuth callback endpoint must be non-zero loopback, got {0}")]
    NonLoopback(std::net::SocketAddr),
    #[error("failed to inspect OAuth listener address: {0}")]
    ListenerAddress(#[source] io::Error),
    #[error("failed to bind OAuth callback listener: {0}")]
    Bind(#[source] io::Error),
    #[error("OAuth callback relay failed: {0}")]
    Relay(#[source] RelayError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn validates_http_urls_without_credentials_or_unsafe_schemes() {
        let (_, authority) =
            validate_browser_url("https://Auth.Example.com/oauth?state=secret#fragment").unwrap();
        assert_eq!(authority.to_string(), "auth.example.com:443");
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pass@example.com/",
            "https:///missing-host",
            "https://example.com/\nnext",
        ] {
            assert!(validate_browser_url(url).is_err(), "accepted {url}");
        }
    }

    #[tokio::test]
    async fn browser_client_authenticates_and_sends_bounded_request() {
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        let expected = token.clone();
        let (client, mut server) = duplex(4_096);
        let server = tokio::spawn(async move {
            authenticate_server(&mut server, &expected, Duration::from_secs(1))
                .await
                .unwrap();
            let length = server.read_u32().await.unwrap() as usize;
            let mut request = vec![0_u8; length];
            server.read_exact(&mut request).await.unwrap();
            let request: BrowserOpenRequest = serde_json::from_slice(&request).unwrap();
            assert_eq!(request.url, "https://auth.example.com/start?state=secret");
            server.write_all(b"OK\n").await.unwrap();
        });
        request_browser_open_stream(
            client,
            &token,
            "https://auth.example.com/start?state=secret",
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn opener_server_enforces_allowlist_without_networking() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        let client_token = token.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            serve_browser_open(
                listener,
                token,
                BrowserOpenConfig {
                    allowlist: AllowList::parse(["auth.example.com:443"]).unwrap(),
                    opener_program: PathBuf::from("true"),
                    opener_args: Vec::new(),
                    relay: RelayConfig::default(),
                    opener_timeout: Duration::from_secs(1),
                },
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });
        request_browser_open(
            &address.to_string(),
            &client_token,
            "https://auth.example.com/oauth?secret=not-logged",
            &RelayConfig::default(),
        )
        .await
        .unwrap();
        let denied = request_browser_open(
            &address.to_string(),
            &client_token,
            "https://attacker.example/oauth",
            &RelayConfig::default(),
        )
        .await;
        assert!(matches!(denied, Err(BrowserError::ServerDenied)));
        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }

    #[test]
    fn oauth_addresses_must_be_loopback() {
        assert!(require_loopback("127.0.0.1:1455".parse().unwrap()).is_ok());
        assert!(require_loopback("[::1]:1455".parse().unwrap()).is_ok());
        assert!(matches!(
            require_loopback("0.0.0.0:1455".parse().unwrap()),
            Err(OAuthTunnelError::NonLoopback(_))
        ));
        assert!(matches!(
            require_loopback("127.0.0.1:0".parse().unwrap()),
            Err(OAuthTunnelError::NonLoopback(_))
        ));
    }

    #[tokio::test]
    async fn reverse_oauth_tunnel_relays_callback_end_to_end() {
        let callback = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let callback_address = callback.local_addr().unwrap();
        let callback_task = tokio::spawn(async move {
            let (mut stream, _) = callback.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            stream.write_all(&request).await.unwrap();
        });

        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let host_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host_address = host_listener.local_addr().unwrap();
        let token = AuthToken::new(b"oauth-secret".to_vec()).unwrap();
        let target_token = token.clone();
        let (target_shutdown_tx, target_shutdown_rx) = tokio::sync::oneshot::channel();
        let target_task = tokio::spawn(async move {
            serve_oauth_callback_target(
                target_listener,
                callback_address,
                target_token,
                RelayConfig::default(),
                async {
                    let _ = target_shutdown_rx.await;
                },
            )
            .await
        });
        let (host_shutdown_tx, host_shutdown_rx) = tokio::sync::oneshot::channel();
        let host_task = tokio::spawn(async move {
            serve_oauth_host_listener(
                host_listener,
                target_address.to_string(),
                token,
                RelayConfig::default(),
                async {
                    let _ = host_shutdown_rx.await;
                },
            )
            .await
        });

        let mut browser = TcpStream::connect(host_address).await.unwrap();
        browser
            .write_all(b"GET /callback?code=one HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        browser.shutdown().await.unwrap();
        let mut echoed = Vec::new();
        browser.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"GET /callback?code=one HTTP/1.1\r\n\r\n");

        callback_task.await.unwrap();
        let _ = host_shutdown_tx.send(());
        let _ = target_shutdown_tx.send(());
        host_task.await.unwrap().unwrap();
        target_task.await.unwrap().unwrap();
    }
}
