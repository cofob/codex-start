//! Authenticated TCP and Unix-socket relays with bounded resource use.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{sleep, timeout},
};
use tracing::warn;

use crate::auth::{AuthToken, MAX_TOKEN_BYTES};

const HANDSHAKE_MAGIC: &[u8; 8] = b"CDXSRLY1";
const AUTH_OK: u8 = 1;
const AUTH_DENIED: u8 = 0;

/// Limits and timeouts shared by relay server modes.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Maximum simultaneously active connections.
    pub max_connections: usize,
    /// Time allowed to establish an upstream connection.
    pub connect_timeout: Duration,
    /// Time allowed to complete the authentication handshake.
    pub handshake_timeout: Duration,
    /// Close a relay when neither direction transfers bytes for this duration.
    pub idle_timeout: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_connections: 128,
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

impl RelayConfig {
    fn validate(&self) -> Result<(), RelayError> {
        if self.max_connections == 0 {
            return Err(RelayError::InvalidConfig(
                "max_connections must be non-zero".to_owned(),
            ));
        }
        if self.connect_timeout.is_zero()
            || self.handshake_timeout.is_zero()
            || self.idle_timeout.is_zero()
        {
            return Err(RelayError::InvalidConfig(
                "relay timeouts must be non-zero".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A relay destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayTarget {
    /// A TCP destination accepted by `tokio::net::TcpStream::connect`.
    Tcp(String),
    /// A Unix-domain socket.
    Unix(PathBuf),
}

/// Byte counts returned after both relay directions close cleanly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelayStats {
    /// Bytes copied from the first stream to the second.
    pub first_to_second: u64,
    /// Bytes copied from the second stream to the first.
    pub second_to_first: u64,
}

/// Runs a TCP authentication server and forwards accepted streams to `target`.
///
/// # Errors
///
/// Returns an error for invalid limits or a listener-level accept failure.
/// Connection-level authentication and transport failures are logged.
pub async fn serve_authenticated_tcp<F>(
    listener: TcpListener,
    target: RelayTarget,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), RelayError>
where
    F: Future<Output = ()>,
{
    config.validate()?;
    let config = Arc::new(config);
    let token = Arc::new(token);
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer) = accepted.map_err(RelayError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "relay connection denied: capacity reached");
                    continue;
                };
                let token = Arc::clone(&token);
                let target = target.clone();
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_authenticated_server(stream, &target, &token, &config).await {
                        warn!(%peer, %error, "relay connection failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "relay task panicked or was cancelled");
                }
            }
        }
    }

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_authenticated_server(
    mut inbound: TcpStream,
    target: &RelayTarget,
    token: &AuthToken,
    config: &RelayConfig,
) -> Result<RelayStats, RelayError> {
    authenticate_server(&mut inbound, token, config.handshake_timeout).await?;
    match target {
        RelayTarget::Tcp(target) => {
            let mut outbound = timeout(config.connect_timeout, TcpStream::connect(target))
                .await
                .map_err(|_| RelayError::ConnectTimeout(target.clone()))?
                .map_err(|source| RelayError::Connect {
                    target: target.clone(),
                    source,
                })?;
            relay_bidirectional(&mut inbound, &mut outbound, config.idle_timeout).await
        }
        RelayTarget::Unix(path) => {
            let mut outbound = timeout(config.connect_timeout, UnixStream::connect(path))
                .await
                .map_err(|_| RelayError::ConnectTimeout(path.display().to_string()))?
                .map_err(|source| RelayError::Connect {
                    target: path.display().to_string(),
                    source,
                })?;
            relay_bidirectional(&mut inbound, &mut outbound, config.idle_timeout).await
        }
    }
}

/// Runs a local TCP bridge whose outbound side uses the authenticated protocol.
///
/// # Errors
///
/// Returns an error for invalid limits or a listener-level accept failure.
pub async fn serve_tcp_bridge<F>(
    listener: TcpListener,
    remote: String,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), RelayError>
where
    F: Future<Output = ()>,
{
    serve_client_bridge(
        LocalListener::Tcp(listener),
        remote,
        token,
        config,
        shutdown,
    )
    .await
}

/// Runs a local Unix-socket bridge whose outbound side uses the authenticated protocol.
///
/// # Errors
///
/// Returns an error for invalid limits or a listener-level accept failure.
pub async fn serve_unix_bridge<F>(
    listener: UnixListener,
    remote: String,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), RelayError>
where
    F: Future<Output = ()>,
{
    serve_client_bridge(
        LocalListener::Unix(listener),
        remote,
        token,
        config,
        shutdown,
    )
    .await
}

enum LocalListener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

enum LocalStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

async fn serve_client_bridge<F>(
    listener: LocalListener,
    remote: String,
    token: AuthToken,
    config: RelayConfig,
    shutdown: F,
) -> Result<(), RelayError>
where
    F: Future<Output = ()>,
{
    config.validate()?;
    let config = Arc::new(config);
    let token = Arc::new(token);
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = accept_local(&listener) => {
                let stream = accepted?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!("local relay connection denied: capacity reached");
                    continue;
                };
                let remote = remote.clone();
                let token = Arc::clone(&token);
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_client_bridge(stream, &remote, &token, &config).await {
                        warn!(%error, "local relay connection failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "relay task panicked or was cancelled");
                }
            }
        }
    }

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn accept_local(listener: &LocalListener) -> Result<LocalStream, RelayError> {
    match listener {
        LocalListener::Tcp(listener) => listener
            .accept()
            .await
            .map(|(stream, _)| LocalStream::Tcp(stream))
            .map_err(RelayError::Accept),
        LocalListener::Unix(listener) => listener
            .accept()
            .await
            .map(|(stream, _)| LocalStream::Unix(stream))
            .map_err(RelayError::Accept),
    }
}

async fn handle_client_bridge(
    mut local: LocalStream,
    remote: &str,
    token: &AuthToken,
    config: &RelayConfig,
) -> Result<RelayStats, RelayError> {
    let mut outbound = timeout(config.connect_timeout, TcpStream::connect(remote))
        .await
        .map_err(|_| RelayError::ConnectTimeout(remote.to_owned()))?
        .map_err(|source| RelayError::Connect {
            target: remote.to_owned(),
            source,
        })?;
    authenticate_client(&mut outbound, token, config.handshake_timeout).await?;
    match &mut local {
        LocalStream::Tcp(local) => {
            relay_bidirectional(local, &mut outbound, config.idle_timeout).await
        }
        LocalStream::Unix(local) => {
            relay_bidirectional(local, &mut outbound, config.idle_timeout).await
        }
    }
}

/// Performs the server side of the versioned relay handshake.
///
/// # Errors
///
/// Returns an error on timeout, malformed framing, invalid authentication, or
/// transport failure.
pub async fn authenticate_server<S>(
    stream: &mut S,
    expected: &AuthToken,
    handshake_timeout: Duration,
) -> Result<(), RelayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(handshake_timeout, async {
        let mut magic = [0_u8; HANDSHAKE_MAGIC.len()];
        stream.read_exact(&mut magic).await?;
        if &magic != HANDSHAKE_MAGIC {
            stream.write_all(&[AUTH_DENIED]).await?;
            return Err(RelayError::InvalidHandshake);
        }
        let length = stream.read_u16().await.map(usize::from)?;
        if length == 0 || length > MAX_TOKEN_BYTES {
            stream.write_all(&[AUTH_DENIED]).await?;
            return Err(RelayError::InvalidHandshake);
        }
        let mut candidate = zeroize::Zeroizing::new(vec![0_u8; length]);
        stream.read_exact(&mut candidate).await?;
        if !expected.matches(&candidate) {
            stream.write_all(&[AUTH_DENIED]).await?;
            return Err(RelayError::AuthenticationDenied);
        }
        stream.write_all(&[AUTH_OK]).await?;
        stream.flush().await?;
        Ok::<(), RelayError>(())
    })
    .await
    .map_err(|_| RelayError::HandshakeTimeout)?
}

/// Performs the client side of the versioned relay handshake.
///
/// # Errors
///
/// Returns an error on timeout, server denial, malformed response, or transport
/// failure.
pub async fn authenticate_client<S>(
    stream: &mut S,
    token: &AuthToken,
    handshake_timeout: Duration,
) -> Result<(), RelayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(handshake_timeout, async {
        let length =
            u16::try_from(token.expose().len()).map_err(|_| RelayError::InvalidHandshake)?;
        stream.write_all(HANDSHAKE_MAGIC).await?;
        stream.write_u16(length).await?;
        stream.write_all(token.expose()).await?;
        stream.flush().await?;
        match stream.read_u8().await? {
            AUTH_OK => Ok(()),
            AUTH_DENIED => Err(RelayError::AuthenticationDenied),
            _ => Err(RelayError::InvalidHandshake),
        }
    })
    .await
    .map_err(|_| RelayError::HandshakeTimeout)?
}

/// Copies both directions, propagating half-closes and enforcing an activity timeout.
///
/// # Errors
///
/// Returns an error on invalid timeout, inactivity, or transport failure.
pub async fn relay_bidirectional<A, B>(
    first: &mut A,
    second: &mut B,
    idle_timeout: Duration,
) -> Result<RelayStats, RelayError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (first_read, first_write) = tokio::io::split(first);
    let (second_read, second_write) = tokio::io::split(second);
    relay_halves(
        first_read,
        first_write,
        second_read,
        second_write,
        idle_timeout,
    )
    .await
}

/// Copies between independently owned read/write halves in both directions.
///
/// This variant supports process pipes as well as sockets. EOF from either
/// reader shuts down only the corresponding destination writer.
///
/// # Errors
///
/// Returns an error on invalid timeout, inactivity, or transport failure.
pub async fn relay_halves<FirstRead, FirstWrite, SecondRead, SecondWrite>(
    first_read: FirstRead,
    first_write: FirstWrite,
    second_read: SecondRead,
    second_write: SecondWrite,
    idle_timeout: Duration,
) -> Result<RelayStats, RelayError>
where
    FirstRead: AsyncRead + Unpin,
    FirstWrite: AsyncWrite + Unpin,
    SecondRead: AsyncRead + Unpin,
    SecondWrite: AsyncWrite + Unpin,
{
    if idle_timeout.is_zero() {
        return Err(RelayError::InvalidConfig(
            "idle_timeout must be non-zero".to_owned(),
        ));
    }
    let (activity, activity_rx) = watch::channel(());
    let transfer = transfer_both(first_read, first_write, second_read, second_write, activity);
    tokio::pin!(transfer);
    tokio::select! {
        biased;
        result = &mut transfer => result,
        () = wait_until_idle(activity_rx, idle_timeout) => Err(RelayError::IdleTimeout),
    }
}

/// Relays process-style stdin/stdout to a remote stream.
///
/// Local-input EOF half-closes the remote writer and continues draining remote
/// output. Remote-output EOF ends the relay immediately, even if local input is
/// still open; this matches SSH and `ProxyCommand` lifecycle semantics.
///
/// # Errors
///
/// Returns an error on invalid timeout, inactivity, or transport failure.
pub async fn relay_stdio<Input, Output, RemoteRead, RemoteWrite>(
    input: Input,
    output: Output,
    remote_read: RemoteRead,
    remote_write: RemoteWrite,
    idle_timeout: Duration,
) -> Result<RelayStats, RelayError>
where
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    RemoteRead: AsyncRead + Unpin,
    RemoteWrite: AsyncWrite + Unpin,
{
    if idle_timeout.is_zero() {
        return Err(RelayError::InvalidConfig(
            "idle_timeout must be non-zero".to_owned(),
        ));
    }
    let (activity, activity_rx) = watch::channel(());
    let transfer = transfer_stdio(input, output, remote_read, remote_write, activity);
    tokio::pin!(transfer);
    tokio::select! {
        biased;
        result = &mut transfer => result,
        () = wait_until_idle(activity_rx, idle_timeout) => Err(RelayError::IdleTimeout),
    }
}

async fn transfer_stdio<Input, Output, RemoteRead, RemoteWrite>(
    input: Input,
    output: Output,
    remote_read: RemoteRead,
    remote_write: RemoteWrite,
    activity: watch::Sender<()>,
) -> Result<RelayStats, RelayError>
where
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    RemoteRead: AsyncRead + Unpin,
    RemoteWrite: AsyncWrite + Unpin,
{
    let outbound = pump(input, remote_write, activity.clone());
    let inbound = pump(remote_read, output, activity);
    tokio::pin!(outbound);
    tokio::pin!(inbound);
    tokio::select! {
        biased;
        inbound = &mut inbound => {
            let inbound = inbound?;
            // Flush input that is already buffered when the remote process
            // closes stdout, but do not wait indefinitely for a parent that
            // intentionally keeps stdin open.
            let outbound = timeout(Duration::from_millis(50), &mut outbound)
                .await
                .map_or(Ok(0), |result| result)?;
            Ok(RelayStats {
                first_to_second: outbound,
                second_to_first: inbound,
            })
        },
        outbound = &mut outbound => Ok(RelayStats {
            first_to_second: outbound?,
            second_to_first: inbound.await?,
        }),
    }
}

async fn transfer_both<FirstRead, FirstWrite, SecondRead, SecondWrite>(
    first_read: FirstRead,
    first_write: FirstWrite,
    second_read: SecondRead,
    second_write: SecondWrite,
    activity: watch::Sender<()>,
) -> Result<RelayStats, RelayError>
where
    FirstRead: AsyncRead + Unpin,
    FirstWrite: AsyncWrite + Unpin,
    SecondRead: AsyncRead + Unpin,
    SecondWrite: AsyncWrite + Unpin,
{
    let first_activity = activity.clone();
    let first_to_second = pump(first_read, second_write, first_activity);
    let second_to_first = pump(second_read, first_write, activity);
    let (first_to_second, second_to_first) = tokio::try_join!(first_to_second, second_to_first)?;
    Ok(RelayStats {
        first_to_second,
        second_to_first,
    })
}

async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    activity: watch::Sender<()>,
) -> Result<u64, RelayError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 32 * 1_024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            writer.shutdown().await?;
            return Ok(copied);
        }
        writer.write_all(&buffer[..count]).await?;
        copied = copied.saturating_add(count as u64);
        let _ = activity.send(());
    }
}

async fn wait_until_idle(mut activity: watch::Receiver<()>, idle_timeout: Duration) {
    loop {
        tokio::select! {
            () = sleep(idle_timeout) => return,
            result = activity.changed() => {
                if result.is_err() {
                    // Both pumps are gone; the transfer branch will be selected next.
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

/// Safely creates a Unix listener, replacing only an existing socket node.
///
/// # Errors
///
/// Returns an error when the path is occupied by a non-socket or socket setup
/// and permission changes fail.
pub fn bind_unix_listener(path: &Path) -> Result<UnixListener, RelayError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|source| RelayError::BindUnix {
                path: path.to_owned(),
                source,
            })?;
        }
        Ok(_) => return Err(RelayError::RefuseReplace(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RelayError::BindUnix {
                path: path.to_owned(),
                source,
            });
        }
    }
    let listener = UnixListener::bind(path).map_err(|source| RelayError::BindUnix {
        path: path.to_owned(),
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        RelayError::BindUnix {
            path: path.to_owned(),
            source,
        }
    })?;
    Ok(listener)
}

/// Relay protocol and transport errors.
#[derive(Debug, Error)]
pub enum RelayError {
    #[error("invalid relay configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to accept relay connection: {0}")]
    Accept(#[source] io::Error),
    #[error("timed out connecting to {0}")]
    ConnectTimeout(String),
    #[error("failed to connect to {target}: {source}")]
    Connect {
        target: String,
        #[source]
        source: io::Error,
    },
    #[error("relay authentication timed out")]
    HandshakeTimeout,
    #[error("invalid relay handshake")]
    InvalidHandshake,
    #[error("relay authentication denied")]
    AuthenticationDenied,
    #[error("relay was idle for too long")]
    IdleTimeout,
    #[error("relay I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("failed to bind Unix socket {path}: {source}")]
    BindUnix {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("refusing to replace non-socket path {0}")]
    RefuseReplace(PathBuf),
}

/// Parses and validates a literal socket address for listener arguments.
///
/// # Errors
///
/// Returns an error when `input` is not a literal IP address and port.
pub fn parse_listen_address(input: &str) -> Result<SocketAddr, RelayError> {
    input.parse().map_err(|_| {
        RelayError::InvalidConfig(format!(
            "listener `{input}` must be a literal IP address and port"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn authentication_round_trip_and_denial() {
        let expected = AuthToken::new(b"expected".to_vec()).unwrap();
        let correct = expected.clone();
        let (mut client, mut server) = duplex(256);
        let (client_result, server_result) = tokio::join!(
            authenticate_client(&mut client, &correct, Duration::from_secs(1)),
            authenticate_server(&mut server, &expected, Duration::from_secs(1))
        );
        client_result.unwrap();
        server_result.unwrap();

        let wrong = AuthToken::new(b"wrong".to_vec()).unwrap();
        let (mut client, mut server) = duplex(256);
        let (client_result, server_result) = tokio::join!(
            authenticate_client(&mut client, &wrong, Duration::from_secs(1)),
            authenticate_server(&mut server, &expected, Duration::from_secs(1))
        );
        assert!(matches!(
            client_result,
            Err(RelayError::AuthenticationDenied)
        ));
        assert!(matches!(
            server_result,
            Err(RelayError::AuthenticationDenied)
        ));
    }

    #[tokio::test]
    async fn relay_preserves_half_close_and_counts_bytes() {
        let (mut first_client, mut first_relay) = duplex(1_024);
        let (mut second_relay, mut second_client) = duplex(1_024);
        let relay = tokio::spawn(async move {
            relay_bidirectional(&mut first_relay, &mut second_relay, Duration::from_secs(2)).await
        });

        first_client.write_all(b"request").await.unwrap();
        first_client.shutdown().await.unwrap();
        let mut request = Vec::new();
        second_client.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");

        second_client.write_all(b"response").await.unwrap();
        second_client.shutdown().await.unwrap();
        let mut response = Vec::new();
        first_client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");

        let stats = relay.await.unwrap().unwrap();
        assert_eq!(stats.first_to_second, 7);
        assert_eq!(stats.second_to_first, 8);
    }

    #[tokio::test]
    async fn idle_timeout_closes_quiet_relay() {
        let (_first_client, mut first_relay) = duplex(64);
        let (_second_client, mut second_relay) = duplex(64);
        let result = relay_bidirectional(
            &mut first_relay,
            &mut second_relay,
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(result, Err(RelayError::IdleTimeout)));
    }

    #[tokio::test]
    async fn stdio_relay_exits_when_remote_output_closes_even_if_input_stays_open() {
        let (_input_writer, input_reader) = duplex(64);
        let (output_writer, mut output_reader) = duplex(64);
        let (remote_read, mut remote_writer) = duplex(64);
        let (remote_write, _remote_reader) = duplex(64);
        remote_writer.write_all(b"done").await.unwrap();
        remote_writer.shutdown().await.unwrap();
        relay_stdio(
            input_reader,
            output_writer,
            remote_read,
            remote_write,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"done");
    }

    #[test]
    fn unix_bind_refuses_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay.sock");
        std::fs::write(&path, b"do not remove").unwrap();
        assert!(matches!(
            bind_unix_listener(&path),
            Err(RelayError::RefuseReplace(_))
        ));
    }
}
