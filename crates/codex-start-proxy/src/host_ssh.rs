//! Authenticated, allow-listed execution of the host OpenSSH client.

use std::{future::Future, io, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    allowlist::{AllowList, AllowlistError, Authority, NormalizedHost},
    auth::AuthToken,
    relay::{RelayConfig, RelayError, authenticate_client, authenticate_server},
};

const MAX_REQUEST_BYTES: usize = 64 * 1_024;
const MAX_ARGUMENTS: usize = 256;
const MAX_RESPONSE_LINE: usize = 1_024;
const OUTPUT_CHUNK_BYTES: usize = 16 * 1_024;
const FRAME_STDOUT: u8 = 1;
const FRAME_STDERR: u8 = 2;
const FRAME_EXIT: u8 = 3;

/// Git service requested through the host OpenSSH client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitSshService {
    /// Fetch objects and refs from a repository.
    UploadPack,
    /// Push objects and refs to a repository.
    ReceivePack,
    /// Produce a Git archive from a repository.
    UploadArchive,
    /// Obtain scoped Git LFS credentials for an upload or download.
    LfsAuthenticate,
}

/// A strictly validated remote Git service command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRemoteCommand {
    /// Requested Git service.
    pub service: GitSshService,
    /// Repository argument, with its optional single quotes removed.
    pub repository: String,
    /// Git LFS operation. This is present only for `git-lfs-authenticate`.
    pub lfs_operation: Option<GitLfsOperation>,
}

/// Operation accepted by `git-lfs-authenticate`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitLfsOperation {
    /// Download Git LFS objects.
    Download,
    /// Upload Git LFS objects.
    Upload,
}

/// Parsed destination and exact argv approved for host execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshInvocation {
    /// Actual hostname after applying an allowed `HostName` option.
    pub authority: Authority,
    /// Original validated OpenSSH arguments.
    pub argv: Vec<String>,
    /// Remote Git service command approved for execution.
    pub remote_command: GitRemoteCommand,
}

impl SshInvocation {
    /// Strictly parses the subset of OpenSSH options needed by Git transports.
    ///
    /// # Errors
    ///
    /// Returns an error for missing destinations, invalid ports, excessive
    /// input, or options capable of forwarding, executing local commands, or
    /// accessing caller-selected host files.
    pub fn parse(argv: Vec<String>) -> Result<Self, HostSshError> {
        if argv.is_empty() || argv.len() > MAX_ARGUMENTS {
            return Err(HostSshError::InvalidInvocation(
                "SSH argv is empty or has too many arguments".to_owned(),
            ));
        }
        if argv
            .iter()
            .any(|argument| argument.is_empty() || argument.chars().any(char::is_control))
        {
            return Err(HostSshError::InvalidInvocation(
                "SSH argv contains an empty argument or control byte".to_owned(),
            ));
        }
        let encoded_size = argv.iter().map(String::len).sum::<usize>();
        if encoded_size > MAX_REQUEST_BYTES {
            return Err(HostSshError::RequestTooLarge);
        }

        let mut destination_host = None;
        let mut destination_index = None;
        let mut hostname_override = None;
        let mut port = 22_u16;
        let mut index = 0;
        while index < argv.len() {
            let argument = &argv[index];
            if argument == "--" {
                index += 1;
                let destination = argv.get(index).ok_or_else(|| {
                    HostSshError::InvalidInvocation("missing SSH destination after --".to_owned())
                })?;
                destination_host = Some(parse_destination(destination)?);
                destination_index = Some(index);
                break;
            }
            if !argument.starts_with('-') || argument == "-" {
                destination_host = Some(parse_destination(argument)?);
                destination_index = Some(index);
                break;
            }

            match argument.as_str() {
                "-p" => {
                    port = parse_port(next_value(&argv, &mut index, "-p")?)?;
                }
                "-l" => {
                    validate_user(next_value(&argv, &mut index, "-l")?)?;
                }
                "-o" => {
                    parse_option(
                        next_value(&argv, &mut index, "-o")?,
                        &mut hostname_override,
                        &mut port,
                    )?;
                }
                // These flags affect transport presentation only and cannot
                // open listeners, execute host commands, or select host files.
                "-4" | "-6" | "-a" | "-C" | "-n" | "-q" | "-T" | "-v" | "-vv" | "-vvv" | "-x" => {}
                _ if argument.starts_with("-p") && argument.len() > 2 => {
                    port = parse_port(&argument[2..])?;
                }
                _ if argument.starts_with("-l") && argument.len() > 2 => {
                    validate_user(&argument[2..])?;
                }
                _ if argument.starts_with("-o") && argument.len() > 2 => {
                    parse_option(&argument[2..], &mut hostname_override, &mut port)?;
                }
                _ => {
                    return Err(HostSshError::UnsafeOption(argument.clone()));
                }
            }
            index += 1;
        }

        let destination_host = destination_host.ok_or_else(|| {
            HostSshError::InvalidInvocation("SSH destination is missing".to_owned())
        })?;
        let destination_index = destination_index.ok_or_else(|| {
            HostSshError::InvalidInvocation("SSH destination is missing".to_owned())
        })?;
        let remote_argv = argv.get(destination_index + 1..).unwrap_or_default();
        if remote_argv.is_empty() {
            return Err(HostSshError::InvalidInvocation(
                "a remote Git service command is required after the SSH destination".to_owned(),
            ));
        }
        let remote_command = parse_git_remote_argv(remote_argv)?;
        let host = hostname_override.unwrap_or(destination_host);
        Ok(Self {
            authority: Authority {
                host: NormalizedHost::parse(&host)?,
                port,
            },
            argv,
            remote_command,
        })
    }
}

/// Recognizes Git's side-effect-free OpenSSH capability probe.
///
/// Git invokes an unknown `GIT_SSH_COMMAND` once as `-G <destination>` and
/// treats a successful exit as confirmation that it supports OpenSSH options.
/// The container helper handles this exact shape locally; it is never sent to
/// the host service or accepted by [`SshInvocation::parse`].
#[must_use]
pub fn is_git_ssh_variant_probe(argv: &[String]) -> bool {
    matches!(argv, [option, destination]
        if option == "-G"
            && !destination.chars().any(char::is_control)
            && parse_destination(destination)
                .and_then(|host| NormalizedHost::parse(&host).map_err(Into::into))
                .is_ok())
}

fn parse_git_remote_argv(argv: &[String]) -> Result<GitRemoteCommand, HostSshError> {
    if let [command] = argv {
        return parse_git_remote_fields(&command.split_ascii_whitespace().collect::<Vec<_>>());
    }
    parse_git_remote_fields(&argv.iter().map(String::as_str).collect::<Vec<_>>())
}

fn parse_git_remote_fields(fields: &[&str]) -> Result<GitRemoteCommand, HostSshError> {
    let service = match fields.first().copied() {
        Some("git-upload-pack") => GitSshService::UploadPack,
        Some("git-receive-pack") => GitSshService::ReceivePack,
        Some("git-upload-archive") => GitSshService::UploadArchive,
        Some("git-lfs-authenticate") => GitSshService::LfsAuthenticate,
        _ => return Err(HostSshError::RemoteCommandDenied),
    };
    let expected_fields = if service == GitSshService::LfsAuthenticate {
        3
    } else {
        2
    };
    if fields.len() != expected_fields {
        return Err(HostSshError::RemoteCommandDenied);
    }

    let repository = parse_repository(fields[1])?;
    let lfs_operation = if service == GitSshService::LfsAuthenticate {
        match fields[2] {
            "download" => Some(GitLfsOperation::Download),
            "upload" => Some(GitLfsOperation::Upload),
            _ => return Err(HostSshError::RemoteCommandDenied),
        }
    } else {
        None
    };
    Ok(GitRemoteCommand {
        service,
        repository,
        lfs_operation,
    })
}

fn parse_repository(value: &str) -> Result<String, HostSshError> {
    let repository = value
        .strip_prefix('\'')
        .and_then(|remainder| remainder.strip_suffix('\''))
        .unwrap_or(value);
    if repository.is_empty()
        || repository.starts_with('-')
        || repository.split('/').any(|component| component == "..")
        || !repository.bytes().all(is_safe_repository_byte)
        || (value.starts_with('\'') != value.ends_with('\''))
    {
        return Err(HostSshError::RemoteCommandDenied);
    }
    Ok(repository.to_owned())
}

const fn is_safe_repository_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'/' | b'.' | b'_' | b'-' | b'~' | b'@' | b'%' | b'+' | b':' | b'=' | b','
        )
}

fn next_value<'a>(
    argv: &'a [String],
    index: &mut usize,
    option: &str,
) -> Result<&'a str, HostSshError> {
    *index += 1;
    argv.get(*index).map(String::as_str).ok_or_else(|| {
        HostSshError::InvalidInvocation(format!("SSH option {option} requires a value"))
    })
}

fn parse_destination(destination: &str) -> Result<String, HostSshError> {
    let host = if let Some((user, host)) = destination.rsplit_once('@') {
        validate_user(user)?;
        if user.contains('@') {
            return Err(HostSshError::InvalidInvocation(
                "invalid SSH destination user".to_owned(),
            ));
        }
        host
    } else {
        destination
    };
    let host = match host.strip_prefix('[') {
        Some(remainder) => remainder.strip_suffix(']').ok_or_else(|| {
            HostSshError::InvalidInvocation("invalid bracketed SSH destination".to_owned())
        })?,
        None => host,
    };
    if host.is_empty() {
        return Err(HostSshError::InvalidInvocation(
            "SSH destination host is empty".to_owned(),
        ));
    }
    Ok(host.to_owned())
}

fn parse_port(value: &str) -> Result<u16, HostSshError> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| HostSshError::InvalidInvocation(format!("invalid SSH port `{value}`")))
}

fn validate_user(value: &str) -> Result<(), HostSshError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(HostSshError::InvalidInvocation(
            "invalid SSH user".to_owned(),
        ));
    }
    Ok(())
}

fn parse_option(
    option: &str,
    hostname: &mut Option<String>,
    port: &mut u16,
) -> Result<(), HostSshError> {
    let (name, value) = option.split_once('=').ok_or_else(|| {
        HostSshError::InvalidInvocation("SSH -o options must use name=value".to_owned())
    })?;
    match name.to_ascii_lowercase().as_str() {
        "hostname" if !value.contains('@') => *hostname = Some(parse_destination(value)?),
        "port" => *port = parse_port(value)?,
        "user" => validate_user(value)?,
        "batchmode" if value.eq_ignore_ascii_case("yes") => {}
        "sendenv" if value == "GIT_PROTOCOL" => {}
        // All other -o values are rejected. In particular this prevents
        // ProxyCommand, LocalCommand, Include, control sockets, and arbitrary
        // host-side file reads/writes.
        _ => return Err(HostSshError::UnsafeOption(format!("-o{option}"))),
    }
    Ok(())
}

/// Host SSH service configuration.
#[derive(Clone, Debug)]
pub struct HostSshConfig {
    /// Permitted SSH destination authorities. Rules should normally include
    /// explicit port 22 (for example `github.com:22`).
    pub allowlist: AllowList,
    /// OpenSSH client executable on the host.
    pub ssh_program: PathBuf,
    /// Connection limits and authentication/idle timeouts.
    pub relay: RelayConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SshRequest {
    argv: Vec<String>,
}

/// Serves authenticated host-SSH requests until `shutdown` resolves.
///
/// # Errors
///
/// Returns an error for an invalid configuration or listener-level failure.
/// Request-level denials and child-process errors are isolated and logged.
pub async fn serve_host_ssh<F>(
    listener: TcpListener,
    token: AuthToken,
    config: HostSshConfig,
    shutdown: F,
) -> Result<(), HostSshError>
where
    F: Future<Output = ()>,
{
    if config.relay.max_connections == 0
        || config.relay.handshake_timeout.is_zero()
        || config.relay.idle_timeout.is_zero()
    {
        return Err(HostSshError::InvalidConfig);
    }
    let config = Arc::new(config);
    let token = Arc::new(token);
    let semaphore = Arc::new(Semaphore::new(config.relay.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer) = accepted.map_err(HostSshError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, "host SSH request denied: capacity reached");
                    continue;
                };
                let token = Arc::clone(&token);
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = handle_server_request(stream, &token, &config).await {
                        warn!(%peer, %error, "host SSH request failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "host SSH task panicked or was cancelled");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_server_request(
    mut stream: TcpStream,
    token: &AuthToken,
    config: &HostSshConfig,
) -> Result<(), HostSshError> {
    authenticate_server(&mut stream, token, config.relay.handshake_timeout).await?;
    let request = match read_request(&mut stream, config.relay.handshake_timeout).await {
        Ok(request) => request,
        Err(error) => {
            send_denied(&mut stream).await;
            return Err(error);
        }
    };
    let invocation = match SshInvocation::parse(request.argv) {
        Ok(invocation) if config.allowlist.allows(&invocation.authority) => invocation,
        Ok(invocation) => {
            send_denied(&mut stream).await;
            return Err(HostSshError::DestinationDenied(invocation.authority));
        }
        Err(error) => {
            send_denied(&mut stream).await;
            return Err(error);
        }
    };

    let mut child = Command::new(&config.ssh_program)
        .args(["-T", "-o", "BatchMode=yes"])
        .args(&invocation.argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| HostSshError::Spawn {
            program: config.ssh_program.clone(),
            source,
        })?;
    let child_stdin = child.stdin.take().ok_or(HostSshError::MissingPipe)?;
    let child_stdout = child.stdout.take().ok_or(HostSshError::MissingPipe)?;
    let child_stderr = child.stderr.take().ok_or(HostSshError::MissingPipe)?;
    stream.write_all(b"OK\n").await?;
    stream.flush().await?;
    info!(target = %invocation.authority, "host SSH request allowed");

    let (socket_read, socket_write) = stream.into_split();
    let relay_result = relay_child_streams(
        socket_read,
        socket_write,
        child_stdin,
        child_stdout,
        child_stderr,
        config.relay.idle_timeout,
    )
    .await;
    let mut socket_write = match relay_result {
        Ok(socket_write) => socket_write,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error.into());
        }
    };
    let status = if let Ok(status) = timeout(config.relay.idle_timeout, child.wait()).await {
        status.map_err(HostSshError::Wait)?
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(RelayError::IdleTimeout.into());
    };
    let exit_code = conventional_exit_code(status);
    if exit_code != 0 {
        warn!(target = %invocation.authority, %status, "host SSH exited unsuccessfully");
    }
    write_frame(&mut socket_write, FRAME_EXIT, &[exit_code]).await?;
    socket_write.shutdown().await?;
    Ok(())
}

async fn relay_child_streams<SocketRead, SocketWrite, ChildInput, ChildOutput, ChildError>(
    socket_read: SocketRead,
    socket_write: SocketWrite,
    child_input: ChildInput,
    child_output: ChildOutput,
    child_error: ChildError,
    idle_timeout: Duration,
) -> Result<SocketWrite, RelayError>
where
    SocketRead: AsyncRead + Unpin,
    SocketWrite: AsyncWrite + Unpin,
    ChildInput: AsyncWrite + Unpin,
    ChildOutput: AsyncRead + Unpin,
    ChildError: AsyncRead + Unpin,
{
    if idle_timeout.is_zero() {
        return Err(RelayError::InvalidConfig(
            "idle_timeout must be non-zero".to_owned(),
        ));
    }
    let (activity, activity_rx) = watch::channel(());
    let transfer = transfer_child_streams(
        socket_read,
        socket_write,
        child_input,
        child_output,
        child_error,
        activity,
    );
    tokio::pin!(transfer);
    tokio::select! {
        biased;
        result = &mut transfer => result,
        () = wait_until_idle(activity_rx, idle_timeout) => Err(RelayError::IdleTimeout),
    }
}

async fn transfer_child_streams<SocketRead, SocketWrite, ChildInput, ChildOutput, ChildError>(
    socket_read: SocketRead,
    socket_write: SocketWrite,
    child_input: ChildInput,
    child_output: ChildOutput,
    child_error: ChildError,
    activity: watch::Sender<()>,
) -> Result<SocketWrite, RelayError>
where
    SocketRead: AsyncRead + Unpin,
    SocketWrite: AsyncWrite + Unpin,
    ChildInput: AsyncWrite + Unpin,
    ChildOutput: AsyncRead + Unpin,
    ChildError: AsyncRead + Unpin,
{
    let input = pump_input(socket_read, child_input, activity.clone());
    let output = multiplex_child_output(child_output, child_error, socket_write, activity);
    tokio::pin!(input);
    tokio::pin!(output);
    tokio::select! {
        biased;
        result = &mut output => {
            let writer = result?;
            if let Ok(result) = timeout(Duration::from_millis(50), &mut input).await {
                result?;
            }
            Ok(writer)
        }
        result = &mut input => {
            result?;
            output.await
        }
    }
}

async fn pump_input<Reader, Writer>(
    mut reader: Reader,
    mut writer: Writer,
    activity: watch::Sender<()>,
) -> Result<(), RelayError>
where
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        writer.write_all(&buffer[..count]).await?;
        let _ = activity.send(());
    }
}

async fn multiplex_child_output<Output, ErrorOutput, Writer>(
    mut output: Output,
    mut error_output: ErrorOutput,
    mut writer: Writer,
    activity: watch::Sender<()>,
) -> Result<Writer, RelayError>
where
    Output: AsyncRead + Unpin,
    ErrorOutput: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    let mut stderr_buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    while stdout_open || stderr_open {
        tokio::select! {
            result = output.read(&mut stdout_buffer), if stdout_open => {
                let count = result?;
                if count == 0 {
                    stdout_open = false;
                } else {
                    write_frame(&mut writer, FRAME_STDOUT, &stdout_buffer[..count]).await?;
                    let _ = activity.send(());
                }
            }
            result = error_output.read(&mut stderr_buffer), if stderr_open => {
                let count = result?;
                if count == 0 {
                    stderr_open = false;
                } else {
                    write_frame(&mut writer, FRAME_STDERR, &stderr_buffer[..count]).await?;
                    let _ = activity.send(());
                }
            }
        }
    }
    writer.flush().await?;
    Ok(writer)
}

async fn write_frame<Writer>(writer: &mut Writer, kind: u8, payload: &[u8]) -> io::Result<()>
where
    Writer: AsyncWrite + Unpin,
{
    writer.write_u8(kind).await?;
    writer
        .write_u32(u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "host SSH frame is too large")
        })?)
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

fn conventional_exit_code(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(255)
}

async fn wait_until_idle(mut activity: watch::Receiver<()>, idle_timeout: Duration) {
    loop {
        tokio::select! {
            () = sleep(idle_timeout) => return,
            result = activity.changed() => {
                if result.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

async fn read_request(
    stream: &mut TcpStream,
    request_timeout: Duration,
) -> Result<SshRequest, HostSshError> {
    timeout(request_timeout, async {
        let length =
            usize::try_from(stream.read_u32().await?).map_err(|_| HostSshError::RequestTooLarge)?;
        if length == 0 || length > MAX_REQUEST_BYTES {
            return Err(HostSshError::RequestTooLarge);
        }
        let mut bytes = vec![0_u8; length];
        stream.read_exact(&mut bytes).await?;
        serde_json::from_slice(&bytes).map_err(HostSshError::InvalidRequest)
    })
    .await
    .map_err(|_| HostSshError::RequestTimeout)?
}

async fn send_denied(stream: &mut TcpStream) {
    let _ = stream.write_all(b"ERR request denied\n").await;
    let _ = stream.shutdown().await;
}

/// Runs the container side of the host-SSH protocol over process stdin/stdout.
///
/// # Errors
///
/// Returns the conventional OpenSSH child exit code. Returns an error when
/// connecting, authentication, request validation, host denial, or stream
/// forwarding fails.
pub async fn run_host_ssh_client(
    remote: &str,
    token: &AuthToken,
    argv: Vec<String>,
    config: &RelayConfig,
) -> Result<u8, HostSshError> {
    let stream = timeout(config.connect_timeout, TcpStream::connect(remote))
        .await
        .map_err(|_| HostSshError::ConnectTimeout(remote.to_owned()))?
        .map_err(|source| HostSshError::Connect {
            remote: remote.to_owned(),
            source,
        })?;
    host_ssh_client_stream(
        stream,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
        token,
        argv,
        config,
    )
    .await
}

/// Runs a host-SSH client over supplied streams, primarily for embedding/tests.
///
/// # Errors
///
/// Returns the conventional OpenSSH child exit code. Returns an error for
/// invalid argv, authentication/denial, malformed protocol responses, or
/// I/O/idle failures.
pub async fn host_ssh_client_stream<S, Input, Output, ErrorOutput>(
    mut stream: S,
    input: Input,
    output: Output,
    error_output: ErrorOutput,
    token: &AuthToken,
    argv: Vec<String>,
    config: &RelayConfig,
) -> Result<u8, HostSshError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    ErrorOutput: AsyncWrite + Unpin,
{
    // Validate locally as well, so malformed input never reaches the host.
    SshInvocation::parse(argv.clone())?;
    authenticate_client(&mut stream, token, config.handshake_timeout).await?;
    let request = serde_json::to_vec(&SshRequest { argv }).map_err(HostSshError::EncodeRequest)?;
    if request.len() > MAX_REQUEST_BYTES {
        return Err(HostSshError::RequestTooLarge);
    }
    stream
        .write_u32(u32::try_from(request.len()).map_err(|_| HostSshError::RequestTooLarge)?)
        .await?;
    stream.write_all(&request).await?;
    stream.flush().await?;
    let response = read_response_line(&mut stream, config.handshake_timeout).await?;
    if response != b"OK" {
        return Err(HostSshError::ServerDenied);
    }
    let (socket_read, socket_write) = tokio::io::split(stream);
    relay_client_streams(
        input,
        output,
        error_output,
        socket_read,
        socket_write,
        config.idle_timeout,
    )
    .await
}

async fn relay_client_streams<Input, Output, ErrorOutput, SocketRead, SocketWrite>(
    input: Input,
    output: Output,
    error_output: ErrorOutput,
    socket_read: SocketRead,
    socket_write: SocketWrite,
    idle_timeout: Duration,
) -> Result<u8, HostSshError>
where
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    ErrorOutput: AsyncWrite + Unpin,
    SocketRead: AsyncRead + Unpin,
    SocketWrite: AsyncWrite + Unpin,
{
    if idle_timeout.is_zero() {
        return Err(RelayError::InvalidConfig("idle_timeout must be non-zero".to_owned()).into());
    }
    let (activity, activity_rx) = watch::channel(());
    let transfer = transfer_client_streams(
        input,
        output,
        error_output,
        socket_read,
        socket_write,
        activity,
    );
    tokio::pin!(transfer);
    tokio::select! {
        biased;
        result = &mut transfer => result,
        () = wait_until_idle(activity_rx, idle_timeout) => Err(RelayError::IdleTimeout.into()),
    }
}

async fn transfer_client_streams<Input, Output, ErrorOutput, SocketRead, SocketWrite>(
    input: Input,
    output: Output,
    error_output: ErrorOutput,
    socket_read: SocketRead,
    socket_write: SocketWrite,
    activity: watch::Sender<()>,
) -> Result<u8, HostSshError>
where
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    ErrorOutput: AsyncWrite + Unpin,
    SocketRead: AsyncRead + Unpin,
    SocketWrite: AsyncWrite + Unpin,
{
    let input = pump_client_input(input, socket_write, activity.clone());
    let output = receive_output_frames(socket_read, output, error_output, activity);
    tokio::pin!(input);
    tokio::pin!(output);
    tokio::select! {
        biased;
        result = &mut output => {
            let exit_code = result?;
            if let Ok(result) = timeout(Duration::from_millis(50), &mut input).await {
                result?;
            }
            Ok(exit_code)
        }
        result = &mut input => {
            result?;
            output.await
        }
    }
}

async fn pump_client_input<Input, SocketWrite>(
    mut input: Input,
    mut socket_write: SocketWrite,
    activity: watch::Sender<()>,
) -> Result<(), HostSshError>
where
    Input: AsyncRead + Unpin,
    SocketWrite: AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            socket_write.shutdown().await?;
            return Ok(());
        }
        socket_write.write_all(&buffer[..count]).await?;
        let _ = activity.send(());
    }
}

async fn receive_output_frames<SocketRead, Output, ErrorOutput>(
    mut socket_read: SocketRead,
    mut output: Output,
    mut error_output: ErrorOutput,
    activity: watch::Sender<()>,
) -> Result<u8, HostSshError>
where
    SocketRead: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    ErrorOutput: AsyncWrite + Unpin,
{
    loop {
        let kind = socket_read.read_u8().await?;
        let length = usize::try_from(socket_read.read_u32().await?)
            .map_err(|_| HostSshError::InvalidFrame)?;
        match kind {
            FRAME_STDOUT | FRAME_STDERR if (1..=OUTPUT_CHUNK_BYTES).contains(&length) => {
                let mut payload = vec![0_u8; length];
                socket_read.read_exact(&mut payload).await?;
                if kind == FRAME_STDOUT {
                    output.write_all(&payload).await?;
                } else {
                    error_output.write_all(&payload).await?;
                }
                let _ = activity.send(());
            }
            FRAME_EXIT if length == 1 => {
                let exit_code = socket_read.read_u8().await?;
                output.flush().await?;
                error_output.flush().await?;
                return Ok(exit_code);
            }
            _ => return Err(HostSshError::InvalidFrame),
        }
    }
}

async fn read_response_line<S>(
    stream: &mut S,
    response_timeout: Duration,
) -> Result<Vec<u8>, HostSshError>
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
            if line.len() >= MAX_RESPONSE_LINE {
                return Err(HostSshError::InvalidResponse);
            }
            line.push(byte);
        }
    })
    .await
    .map_err(|_| HostSshError::RequestTimeout)?
}

/// Host SSH validation, protocol, and process errors.
#[derive(Debug, Error)]
pub enum HostSshError {
    #[error("invalid host SSH configuration")]
    InvalidConfig,
    #[error("invalid SSH invocation: {0}")]
    InvalidInvocation(String),
    #[error("unsafe SSH option is not allowed: {0}")]
    UnsafeOption(String),
    #[error("remote SSH command is not an allowed Git service invocation")]
    RemoteCommandDenied,
    #[error("host SSH request is too large")]
    RequestTooLarge,
    #[error("host SSH request timed out")]
    RequestTimeout,
    #[error("invalid host SSH request: {0}")]
    InvalidRequest(#[source] serde_json::Error),
    #[error("failed to encode host SSH request: {0}")]
    EncodeRequest(#[source] serde_json::Error),
    #[error("invalid host SSH server response")]
    InvalidResponse,
    #[error("invalid host SSH output frame")]
    InvalidFrame,
    #[error("host SSH server denied the request")]
    ServerDenied,
    #[error("host SSH destination denied: {0}")]
    DestinationDenied(Authority),
    #[error("failed to accept host SSH connection: {0}")]
    Accept(#[source] io::Error),
    #[error("connection to host SSH relay {0} timed out")]
    ConnectTimeout(String),
    #[error("failed to connect to host SSH relay {remote}: {source}")]
    Connect {
        remote: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to spawn host SSH program {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("host SSH child pipe unavailable")]
    MissingPipe,
    #[error("failed waiting for host SSH process: {0}")]
    Wait(#[source] io::Error),
    #[error("invalid SSH destination: {0}")]
    Allowlist(#[from] AllowlistError),
    #[error("host SSH relay failed: {0}")]
    Relay(#[from] RelayError),
    #[error("host SSH I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn parses_git_ssh_invocation_and_actual_destination() {
        let parsed = SshInvocation::parse(vec![
            "-o".to_owned(),
            "SendEnv=GIT_PROTOCOL".to_owned(),
            "-oHostname=ssh.github.com".to_owned(),
            "-p443".to_owned(),
            "git@github.com".to_owned(),
            "git-upload-pack 'owner/repo.git'".to_owned(),
        ])
        .unwrap();
        assert_eq!(parsed.authority.to_string(), "ssh.github.com:443");
        assert_eq!(
            parsed.remote_command,
            GitRemoteCommand {
                service: GitSshService::UploadPack,
                repository: "owner/repo.git".to_owned(),
                lfs_operation: None,
            }
        );
    }

    #[test]
    fn permits_only_supported_git_remote_services() {
        for (command, service, operation) in [
            ("git-upload-pack repo.git", GitSshService::UploadPack, None),
            (
                "git-receive-pack '/srv/git/repo.git'",
                GitSshService::ReceivePack,
                None,
            ),
            (
                "git-upload-archive org/repo.git",
                GitSshService::UploadArchive,
                None,
            ),
            (
                "git-lfs-authenticate org/repo.git download",
                GitSshService::LfsAuthenticate,
                Some(GitLfsOperation::Download),
            ),
            (
                "git-lfs-authenticate org/repo.git upload",
                GitSshService::LfsAuthenticate,
                Some(GitLfsOperation::Upload),
            ),
        ] {
            let parsed =
                SshInvocation::parse(vec!["git@example.com".to_owned(), command.to_owned()])
                    .unwrap();
            assert_eq!(parsed.remote_command.service, service);
            assert_eq!(parsed.remote_command.lfs_operation, operation);
        }

        let split_lfs = SshInvocation::parse(vec![
            "git@example.com".to_owned(),
            "git-lfs-authenticate".to_owned(),
            "org/repo.git".to_owned(),
            "download".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            split_lfs.remote_command.service,
            GitSshService::LfsAuthenticate
        );
        assert_eq!(
            split_lfs.remote_command.lfs_operation,
            Some(GitLfsOperation::Download)
        );

        let split_git = SshInvocation::parse(vec![
            "git@example.com".to_owned(),
            "git-upload-pack".to_owned(),
            "repo.git".to_owned(),
        ])
        .unwrap();
        assert_eq!(split_git.remote_command.service, GitSshService::UploadPack);
    }

    #[test]
    fn rejects_arbitrary_or_shell_composed_remote_commands() {
        for command in [
            "sh -c id",
            "git-upload-pack",
            "git-upload-pack repo.git extra",
            "git-upload-pack repo.git;id",
            "git-upload-pack $(id)",
            "git-upload-pack '../secrets'",
            "git-upload-pack --help",
            "git-upload-pack 'repo with spaces.git'",
            "git-lfs-authenticate repo.git delete",
            "git-lfs-authenticate repo.git download;id",
            "git-lfs-transfer repo.git download",
        ] {
            let result =
                SshInvocation::parse(vec!["git@example.com".to_owned(), command.to_owned()]);
            assert!(
                matches!(result, Err(HostSshError::RemoteCommandDenied)),
                "unexpected result for {command:?}: {result:?}"
            );
        }

        let unsafe_split_command = SshInvocation::parse(vec![
            "git@example.com".to_owned(),
            "git-upload-pack".to_owned(),
            "repo.git;id".to_owned(),
        ]);
        assert!(matches!(
            unsafe_split_command,
            Err(HostSshError::RemoteCommandDenied)
        ));
    }

    #[test]
    fn recognizes_only_the_exact_local_git_ssh_variant_probe() {
        assert!(is_git_ssh_variant_probe(&[
            "-G".to_owned(),
            "git@example.com".to_owned(),
        ]));
        for argv in [
            vec!["-G".to_owned()],
            vec!["-G".to_owned(), "user@".to_owned()],
            vec![
                "-G".to_owned(),
                "example.com".to_owned(),
                "git-upload-pack repo".to_owned(),
            ],
            vec!["-T".to_owned(), "example.com".to_owned()],
        ] {
            assert!(!is_git_ssh_variant_probe(&argv));
        }
    }

    #[test]
    fn rejects_options_with_host_side_effects() {
        for arguments in [
            vec!["-L8080:localhost:80", "github.com", "git-upload-pack repo"],
            vec![
                "-oProxyCommand=touch /tmp/pwn",
                "github.com",
                "git-upload-pack repo",
            ],
            vec![
                "-F",
                "/workspace/attacker.conf",
                "github.com",
                "git-upload-pack repo",
            ],
            vec!["-E/tmp/host-file", "github.com", "git-upload-pack repo"],
            vec!["-J", "bastion", "github.com", "git-upload-pack repo"],
        ] {
            let argv = arguments.into_iter().map(str::to_owned).collect();
            assert!(SshInvocation::parse(argv).is_err());
        }
    }

    #[test]
    fn rejects_missing_invalid_and_control_destinations() {
        for arguments in [
            vec![],
            vec!["-p", "0", "github.com", "git-upload-pack repo"],
            vec!["-p", "not-a-port", "github.com", "git-upload-pack repo"],
            vec!["user@", "git-upload-pack repo"],
            vec!["@github.com", "git-upload-pack repo"],
            vec!["user@alias@github.com", "git-upload-pack repo"],
            vec!["github.com\nother", "git-upload-pack repo"],
            vec!["-oHostname=git@github.com", "alias", "git-upload-pack repo"],
            vec!["-oHostname=github.com", "git-upload-pack repo"],
        ] {
            let argv = arguments.into_iter().map(str::to_owned).collect();
            assert!(SshInvocation::parse(argv).is_err());
        }
    }

    #[tokio::test]
    async fn client_protocol_authenticates_sends_argv_and_relays() {
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        let expected = token.clone();
        let (client_stream, mut server_stream) = duplex(4_096);
        let (mut input_writer, input_reader) = duplex(128);
        let (output_writer, mut output_reader) = duplex(128);
        let (error_writer, mut error_reader) = duplex(128);
        let server = tokio::spawn(async move {
            authenticate_server(&mut server_stream, &expected, Duration::from_secs(1))
                .await
                .unwrap();
            let length = server_stream.read_u32().await.unwrap() as usize;
            let mut request = vec![0_u8; length];
            server_stream.read_exact(&mut request).await.unwrap();
            let request: SshRequest = serde_json::from_slice(&request).unwrap();
            assert_eq!(request.argv, ["git@github.com", "git-upload-pack repo"]);
            server_stream.write_all(b"OK\n").await.unwrap();
            write_frame(&mut server_stream, FRAME_STDOUT, b"remote-out")
                .await
                .unwrap();
            write_frame(&mut server_stream, FRAME_STDERR, b"remote-error")
                .await
                .unwrap();
            write_frame(&mut server_stream, FRAME_EXIT, &[23])
                .await
                .unwrap();
            let mut input = Vec::new();
            server_stream.read_to_end(&mut input).await.unwrap();
            assert_eq!(input, b"local");
        });
        input_writer.write_all(b"local").await.unwrap();
        input_writer.shutdown().await.unwrap();

        let exit_code = host_ssh_client_stream(
            client_stream,
            input_reader,
            output_writer,
            error_writer,
            &token,
            vec![
                "git@github.com".to_owned(),
                "git-upload-pack repo".to_owned(),
            ],
            &RelayConfig {
                handshake_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(1),
                ..RelayConfig::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(exit_code, 23);
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"remote-out");
        let mut error_output = Vec::new();
        error_reader.read_to_end(&mut error_output).await.unwrap();
        assert_eq!(error_output, b"remote-error");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_rejects_malformed_or_oversized_output_frames() {
        for (kind, length) in [(99, 1_u32), (FRAME_STDOUT, 0), (FRAME_STDERR, 65_537)] {
            let (mut remote_writer, remote_reader) = duplex(128);
            let (_input_writer, input_reader) = duplex(16);
            let (output_writer, _output_reader) = duplex(16);
            let (error_writer, _error_reader) = duplex(16);
            remote_writer.write_u8(kind).await.unwrap();
            remote_writer.write_u32(length).await.unwrap();
            let result = relay_client_streams(
                input_reader,
                output_writer,
                error_writer,
                remote_reader,
                tokio::io::sink(),
                Duration::from_secs(1),
            )
            .await;
            assert!(matches!(result, Err(HostSshError::InvalidFrame)));
        }
    }

    #[tokio::test]
    async fn client_preserves_half_close_and_applies_output_backpressure() {
        let token = AuthToken::new(b"backpressure-secret".to_vec()).unwrap();
        let expected = token.clone();
        let (client_stream, mut server_stream) = duplex(128);
        let (mut input_writer, input_reader) = duplex(32);
        let (output_writer, mut output_reader) = duplex(64);
        let (error_writer, mut error_reader) = duplex(64);
        let stdout_payload = vec![b'o'; OUTPUT_CHUNK_BYTES * 3];
        let stderr_payload = vec![b'e'; OUTPUT_CHUNK_BYTES * 2];
        let expected_stdout = stdout_payload.clone();
        let expected_stderr = stderr_payload.clone();

        let server = tokio::spawn(async move {
            authenticate_server(&mut server_stream, &expected, Duration::from_secs(1))
                .await
                .unwrap();
            let length = server_stream.read_u32().await.unwrap() as usize;
            let mut request = vec![0_u8; length];
            server_stream.read_exact(&mut request).await.unwrap();
            server_stream.write_all(b"OK\n").await.unwrap();
            let mut input = Vec::new();
            server_stream.read_to_end(&mut input).await.unwrap();
            assert_eq!(input, b"request");
            for chunk in stdout_payload.chunks(OUTPUT_CHUNK_BYTES) {
                write_frame(&mut server_stream, FRAME_STDOUT, chunk)
                    .await
                    .unwrap();
            }
            for chunk in stderr_payload.chunks(OUTPUT_CHUNK_BYTES) {
                write_frame(&mut server_stream, FRAME_STDERR, chunk)
                    .await
                    .unwrap();
            }
            write_frame(&mut server_stream, FRAME_EXIT, &[0])
                .await
                .unwrap();
        });
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            output_reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            error_reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        input_writer.write_all(b"request").await.unwrap();
        input_writer.shutdown().await.unwrap();

        let exit_code = host_ssh_client_stream(
            client_stream,
            input_reader,
            output_writer,
            error_writer,
            &token,
            vec![
                "git@example.com".to_owned(),
                "git-upload-pack repo.git".to_owned(),
            ],
            &RelayConfig {
                handshake_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(2),
                ..RelayConfig::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(exit_code, 0);
        assert_eq!(stdout_reader.await.unwrap(), expected_stdout);
        assert_eq!(stderr_reader.await.unwrap(), expected_stderr);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_protocol_enforces_idle_timeout() {
        let (_remote_peer, remote_stream) = duplex(16);
        let (_input_writer, input_reader) = duplex(16);
        let result = relay_client_streams(
            input_reader,
            tokio::io::sink(),
            tokio::io::sink(),
            remote_stream,
            tokio::io::sink(),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(
            result,
            Err(HostSshError::Relay(RelayError::IdleTimeout))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn full_service_separates_outputs_and_propagates_child_status() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("ssh-test");
        std::fs::write(
            &program,
            b"#!/bin/sh\ncat >/dev/null\nprintf host-stdout\nprintf host-stderr >&2\nexit 37\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = AuthToken::new(b"integration-secret".to_vec()).unwrap();
        let client_token = token.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_host_ssh(
            listener,
            token,
            HostSshConfig {
                allowlist: AllowList::parse(["github.com:22"]).unwrap(),
                ssh_program: program,
                relay: RelayConfig {
                    handshake_timeout: Duration::from_secs(1),
                    idle_timeout: Duration::from_secs(2),
                    ..RelayConfig::default()
                },
            },
            async {
                let _ = shutdown_rx.await;
            },
        ));

        let stream = TcpStream::connect(address).await.unwrap();
        let (mut input_writer, input_reader) = duplex(64);
        let (output_writer, mut output_reader) = duplex(64);
        let (error_writer, mut error_reader) = duplex(64);
        input_writer.write_all(b"request-body").await.unwrap();
        input_writer.shutdown().await.unwrap();
        let exit_code = host_ssh_client_stream(
            stream,
            input_reader,
            output_writer,
            error_writer,
            &client_token,
            vec![
                "git@github.com".to_owned(),
                "git-upload-pack 'org/repo.git'".to_owned(),
            ],
            &RelayConfig {
                handshake_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(2),
                ..RelayConfig::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(exit_code, 37);

        let mut stdout = Vec::new();
        output_reader.read_to_end(&mut stdout).await.unwrap();
        let mut stderr = Vec::new();
        error_reader.read_to_end(&mut stderr).await.unwrap();
        assert_eq!(stdout, b"host-stdout");
        assert_eq!(stderr, b"host-stderr");
        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn signal_termination_uses_conventional_ssh_failure_status() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(conventional_exit_code(status), 255);
    }
}
