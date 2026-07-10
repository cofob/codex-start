//! HTTP forward and CONNECT egress proxy implementation.

use std::{collections::HashSet, future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, lookup_host},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};
use tracing::{info, warn};
use url::Url;

use crate::{
    allowlist::{AddressPolicy, AllowList, AllowlistError, Authority, NormalizedHost},
    auth::AuthToken,
    relay::{RelayError, relay_bidirectional},
};

const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1_024;
const MAX_DISCARDED_PIPELINE_BYTES: usize = 64 * 1_024;

/// Limits and policies for an egress proxy listener.
#[derive(Clone, Debug)]
pub struct EgressConfig {
    /// Hosts and ports that may be contacted.
    pub allowlist: AllowList,
    /// Rules for destinations resolving to non-public addresses.
    pub address_policy: AddressPolicy,
    /// Optional HTTP `Proxy-Authorization: Bearer` token.
    pub auth_token: Option<AuthToken>,
    /// Maximum active client connections.
    pub max_connections: usize,
    /// Maximum bytes accepted before the terminating HTTP header delimiter.
    pub max_header_bytes: usize,
    /// Time allowed to read one request head.
    pub header_timeout: Duration,
    /// Time allowed for DNS resolution and TCP connection establishment.
    pub connect_timeout: Duration,
    /// Close a tunnel when neither direction transfers data for this duration.
    pub idle_timeout: Duration,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            allowlist: AllowList::default(),
            address_policy: AddressPolicy::default(),
            auth_token: None,
            max_connections: 256,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            header_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

impl EgressConfig {
    fn validate(&self) -> Result<(), EgressError> {
        if self.max_connections == 0 {
            return Err(EgressError::InvalidConfig(
                "max_connections must be non-zero".to_owned(),
            ));
        }
        if self.max_header_bytes < 1_024 {
            return Err(EgressError::InvalidConfig(
                "max_header_bytes must be at least 1024".to_owned(),
            ));
        }
        if self.header_timeout.is_zero()
            || self.connect_timeout.is_zero()
            || self.idle_timeout.is_zero()
        {
            return Err(EgressError::InvalidConfig(
                "proxy timeouts must be non-zero".to_owned(),
            ));
        }
        if self
            .auth_token
            .as_ref()
            .is_some_and(|token| token.expose().iter().any(|byte| !byte.is_ascii_graphic()))
        {
            return Err(EgressError::InvalidConfig(
                "HTTP proxy authentication token must contain printable ASCII without spaces"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// Serves egress requests until `shutdown` resolves.
///
/// # Errors
///
/// Returns an error for invalid configuration or a listener-level accept
/// failure. Individual malformed or denied connections are logged and closed.
pub async fn serve_egress<F>(
    listener: TcpListener,
    config: EgressConfig,
    shutdown: F,
) -> Result<(), EgressError>
where
    F: Future<Output = ()>,
{
    config.validate()?;
    let config = Arc::new(config);
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (mut stream, peer) = accepted.map_err(EgressError::Accept)?;
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    warn!(%peer, reason = "capacity", "egress request denied");
                    tokio::spawn(async move {
                        let _ = write_response(
                            &mut stream,
                            503,
                            "Service Unavailable",
                            "proxy connection capacity reached",
                            &[],
                        ).await;
                    });
                    continue;
                };
                let config = Arc::clone(&config);
                connections.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = Box::pin(handle_client(&mut stream, &config, peer)).await {
                        warn!(%peer, %error, "egress connection failed");
                    }
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed {
                    warn!(%error, "egress task panicked or was cancelled");
                }
            }
        }
    }

    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn handle_client(
    client: &mut TcpStream,
    config: &EgressConfig,
    peer: SocketAddr,
) -> Result<(), EgressError> {
    let received = match read_request_head(client, config).await {
        Ok(received) => received,
        Err(error) => {
            send_proxy_error(client, &error).await?;
            return Err(error);
        }
    };
    let request = match ParsedRequest::parse(&received.head) {
        Ok(request) => request,
        Err(error) => {
            send_proxy_error(client, &error).await?;
            return Err(error);
        }
    };
    if let Err(error) = request.validate_buffered_prefix(&received.remainder) {
        send_proxy_error(client, &error).await?;
        return Err(error);
    }

    if request.is_health_check() {
        write_response(
            client,
            200,
            "OK",
            "{\"status\":\"ok\"}\n",
            &[("Content-Type", "application/json")],
        )
        .await?;
        return Ok(());
    }
    if !request.is_authorized(config.auth_token.as_ref()) {
        let error = EgressError::ProxyAuthenticationRequired;
        send_proxy_error(client, &error).await?;
        warn!(%peer, reason = "authentication", "egress request denied");
        return Err(error);
    }

    let authority = request.authority().clone();
    if !config.allowlist.allows(&authority) {
        let error = EgressError::DestinationDenied(authority.clone());
        send_proxy_error(client, &error).await?;
        warn!(%peer, target = %authority, reason = "allowlist", "egress request denied");
        return Err(error);
    }

    let mut upstream =
        match connect_checked(&authority, &config.address_policy, config.connect_timeout).await {
            Ok(stream) => stream,
            Err(error) => {
                send_proxy_error(client, &error).await?;
                return Err(error);
            }
        };
    info!(%peer, target = %authority, method = %request.method, "egress request allowed");

    match &request.target {
        RequestTarget::Connect(_) => {
            write_connect_established(client).await?;
            if !received.remainder.is_empty() {
                upstream.write_all(&received.remainder).await?;
            }
            relay_bidirectional(client, &mut upstream, config.idle_timeout)
                .await
                .map_err(EgressError::Relay)?;
        }
        RequestTarget::Forward { path, .. } => {
            let forward = request.forward_head(path);
            if let Err(error) = Box::pin(forward_one_request(
                client,
                &mut upstream,
                &forward,
                request.framing,
                received.remainder,
                config,
            ))
            .await
            {
                send_proxy_error(client, &error).await?;
                return Err(error);
            }
            Box::pin(relay_one_response(
                &mut upstream,
                client,
                &request.method,
                config,
            ))
            .await?;
        }
    }
    Ok(())
}

struct ReceivedHead {
    head: Vec<u8>,
    remainder: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageFraming {
    None,
    ContentLength(u64),
    Chunked,
}

async fn read_request_head(
    stream: &mut TcpStream,
    config: &EgressConfig,
) -> Result<ReceivedHead, EgressError> {
    timeout(config.header_timeout, async {
        let mut received = Vec::with_capacity(4_096);
        let mut buffer = [0_u8; 4_096];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Err(EgressError::UnexpectedEof);
            }
            received.extend_from_slice(&buffer[..count]);
            if let Some(end) = find_header_end(&received) {
                if end > config.max_header_bytes {
                    return Err(EgressError::HeaderTooLarge(config.max_header_bytes));
                }
                return Ok(ReceivedHead {
                    head: received[..end].to_vec(),
                    remainder: received[end..].to_vec(),
                });
            }
            if received.len() > config.max_header_bytes {
                return Err(EgressError::HeaderTooLarge(config.max_header_bytes));
            }
        }
    })
    .await
    .map_err(|_| EgressError::HeaderTimeout)?
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

#[derive(Clone, Debug)]
struct Header {
    name: String,
    value: Vec<u8>,
}

#[derive(Clone, Debug)]
enum RequestTarget {
    Connect(Authority),
    Forward { authority: Authority, path: String },
}

#[derive(Clone, Debug)]
struct ParsedRequest {
    method: String,
    version: String,
    target: RequestTarget,
    headers: Vec<Header>,
    framing: MessageFraming,
    origin_form: bool,
}

impl ParsedRequest {
    fn parse(bytes: &[u8]) -> Result<Self, EgressError> {
        if bytes.contains(&b'\0') {
            return Err(EgressError::MalformedRequest(
                "NUL byte in request".to_owned(),
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| EgressError::MalformedRequest("request head is not UTF-8".to_owned()))?;
        if text[..text.len().saturating_sub(2)].contains('\n')
            && text
                .as_bytes()
                .windows(2)
                .any(|window| window[1] == b'\n' && window[0] != b'\r')
        {
            return Err(EgressError::MalformedRequest(
                "bare LF in request head".to_owned(),
            ));
        }
        let mut lines = text.strip_suffix("\r\n\r\n").unwrap_or(text).split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| EgressError::MalformedRequest("missing request line".to_owned()))?;
        let mut request_parts = request_line.split(' ');
        let method = request_parts.next().unwrap_or_default();
        let raw_target = request_parts.next().unwrap_or_default();
        let version = request_parts.next().unwrap_or_default();
        if method.is_empty()
            || raw_target.is_empty()
            || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || request_parts.next().is_some()
            || !method.bytes().all(is_http_token_byte)
        {
            return Err(EgressError::MalformedRequest(
                "invalid HTTP request line".to_owned(),
            ));
        }

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if line.starts_with([' ', '\t']) {
                return Err(EgressError::MalformedRequest(
                    "obsolete folded header".to_owned(),
                ));
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| EgressError::MalformedRequest("header without colon".to_owned()))?;
            if name.is_empty() || !name.bytes().all(is_http_token_byte) {
                return Err(EgressError::MalformedRequest(
                    "invalid HTTP header name".to_owned(),
                ));
            }
            let value = value.trim_matches([' ', '\t']);
            if value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
            {
                return Err(EgressError::MalformedRequest(
                    "invalid control byte in header value".to_owned(),
                ));
            }
            headers.push(Header {
                name: name.to_ascii_lowercase(),
                value: value.as_bytes().to_vec(),
            });
        }
        let framing = validate_framing(&headers)?;
        if headers.iter().any(|header| header.name == "expect") {
            return Err(EgressError::MalformedRequest(
                "Expect requests are not supported by the one-request proxy".to_owned(),
            ));
        }
        if version == "HTTP/1.0" && framing == MessageFraming::Chunked {
            return Err(EgressError::MalformedRequest(
                "chunked request bodies require HTTP/1.1".to_owned(),
            ));
        }
        let host_header = unique_header(&headers, "host")?;
        let target = parse_request_target(method, raw_target, host_header)?;
        if matches!(target, RequestTarget::Connect(_)) && framing != MessageFraming::None {
            return Err(EgressError::MalformedRequest(
                "CONNECT requests must not carry a message body".to_owned(),
            ));
        }
        Ok(Self {
            method: method.to_owned(),
            version: version.to_owned(),
            target,
            headers,
            framing,
            origin_form: raw_target.starts_with('/') || raw_target == "*",
        })
    }

    fn authority(&self) -> &Authority {
        match &self.target {
            RequestTarget::Connect(authority) | RequestTarget::Forward { authority, .. } => {
                authority
            }
        }
    }

    fn is_health_check(&self) -> bool {
        self.method == "GET"
            && self.origin_form
            && matches!(&self.target, RequestTarget::Forward { path, .. } if path == "/healthz")
    }

    fn validate_buffered_prefix(&self, bytes: &[u8]) -> Result<(), EgressError> {
        if matches!(self.target, RequestTarget::Connect(_)) {
            return Ok(());
        }
        let exceeds_frame = match self.framing {
            MessageFraming::None => !bytes.is_empty(),
            MessageFraming::ContentLength(length) => {
                u64::try_from(bytes.len()).unwrap_or(u64::MAX) > length
            }
            MessageFraming::Chunked => false,
        };
        if exceeds_frame {
            return Err(EgressError::MalformedRequest(
                "bytes after the declared request body are forbidden".to_owned(),
            ));
        }
        Ok(())
    }

    fn is_authorized(&self, expected: Option<&AuthToken>) -> bool {
        let Some(expected) = expected else {
            return true;
        };
        let Ok(Some(value)) = unique_header(&self.headers, "proxy-authorization") else {
            return false;
        };
        let Some((scheme, candidate)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer") && expected.matches(candidate.as_bytes())
    }

    fn forward_head(&self, path: &str) -> Vec<u8> {
        let connection_headers = self
            .headers
            .iter()
            .filter(|header| header.name == "connection")
            .flat_map(|header| header.value.split(|byte| *byte == b','))
            .map(trim_ascii)
            .map(|value| String::from_utf8_lossy(value).to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let mut output = format!("{} {path} {}\r\n", self.method, self.version).into_bytes();
        output.extend_from_slice(format!("Host: {}\r\n", self.authority()).as_bytes());
        for header in &self.headers {
            if matches!(
                header.name.as_str(),
                "host"
                    | "connection"
                    | "proxy-connection"
                    | "proxy-authorization"
                    | "keep-alive"
                    | "upgrade"
            ) || connection_headers.contains(&header.name)
            {
                continue;
            }
            output.extend_from_slice(header.name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(&header.value);
            output.extend_from_slice(b"\r\n");
        }
        output.extend_from_slice(b"Connection: close\r\n\r\n");
        output
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseFraming {
    None,
    ContentLength(u64),
    Chunked,
    CloseDelimited,
}

struct ParsedResponse {
    version: String,
    status: u16,
    reason: String,
    headers: Vec<Header>,
    message_framing: MessageFraming,
}

impl ParsedResponse {
    fn parse(bytes: &[u8]) -> Result<Self, EgressError> {
        if bytes.contains(&b'\0') {
            return Err(EgressError::MalformedResponse(
                "NUL byte in response".to_owned(),
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| EgressError::MalformedResponse("response head is not UTF-8".to_owned()))?;
        if bytes.first() == Some(&b'\n')
            || bytes
                .windows(2)
                .any(|window| window[1] == b'\n' && window[0] != b'\r')
        {
            return Err(EgressError::MalformedResponse(
                "bare LF in response head".to_owned(),
            ));
        }
        let mut lines = text.strip_suffix("\r\n\r\n").unwrap_or(text).split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| EgressError::MalformedResponse("missing status line".to_owned()))?;
        let mut status_parts = status_line.splitn(3, ' ');
        let version = status_parts.next().unwrap_or_default();
        let status = status_parts.next().unwrap_or_default();
        let reason = status_parts.next().unwrap_or_default();
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || status.len() != 3
            || !status.bytes().all(|byte| byte.is_ascii_digit())
            || reason.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(EgressError::MalformedResponse(
                "invalid HTTP status line".to_owned(),
            ));
        }
        let status = status
            .parse::<u16>()
            .map_err(|_| EgressError::MalformedResponse("invalid HTTP status".to_owned()))?;
        if !(100..=599).contains(&status) {
            return Err(EgressError::MalformedResponse(
                "HTTP status is out of range".to_owned(),
            ));
        }
        let headers = parse_response_headers(lines)?;
        let message_framing = validate_framing(&headers)
            .map_err(|error| EgressError::MalformedResponse(error.to_string()))?;
        if ((100..200).contains(&status) || status == 204)
            && message_framing != MessageFraming::None
        {
            return Err(EgressError::MalformedResponse(
                "bodyless response has a message-framing header".to_owned(),
            ));
        }
        Ok(Self {
            version: version.to_owned(),
            status,
            reason: reason.to_owned(),
            headers,
            message_framing,
        })
    }

    fn is_informational(&self) -> bool {
        (100..200).contains(&self.status)
    }

    fn framing(&self, request_method: &str) -> ResponseFraming {
        if self.is_informational()
            || request_method.eq_ignore_ascii_case("HEAD")
            || matches!(self.status, 204 | 304)
        {
            return ResponseFraming::None;
        }
        match self.message_framing {
            MessageFraming::None => ResponseFraming::CloseDelimited,
            MessageFraming::ContentLength(length) => ResponseFraming::ContentLength(length),
            MessageFraming::Chunked => ResponseFraming::Chunked,
        }
    }

    fn forward_head(&self, final_response: bool) -> Vec<u8> {
        let connection_headers = self
            .headers
            .iter()
            .filter(|header| header.name == "connection")
            .flat_map(|header| header.value.split(|byte| *byte == b','))
            .map(trim_ascii)
            .map(|value| String::from_utf8_lossy(value).to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let mut output =
            format!("{} {} {}\r\n", self.version, self.status, self.reason).into_bytes();
        for header in &self.headers {
            if matches!(
                header.name.as_str(),
                "connection"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "proxy-connection"
                    | "upgrade"
            ) || connection_headers.contains(&header.name)
            {
                continue;
            }
            output.extend_from_slice(header.name.as_bytes());
            output.extend_from_slice(b": ");
            output.extend_from_slice(&header.value);
            output.extend_from_slice(b"\r\n");
        }
        if final_response {
            output.extend_from_slice(b"Connection: close\r\n");
        }
        output.extend_from_slice(b"\r\n");
        output
    }
}

fn parse_response_headers<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Vec<Header>, EgressError> {
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(EgressError::MalformedResponse(
                "obsolete folded header".to_owned(),
            ));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            EgressError::MalformedResponse("response header without colon".to_owned())
        })?;
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            return Err(EgressError::MalformedResponse(
                "invalid response header name".to_owned(),
            ));
        }
        let value = value.trim_matches([' ', '\t']);
        if value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(EgressError::MalformedResponse(
                "invalid control byte in response header value".to_owned(),
            ));
        }
        headers.push(Header {
            name: name.to_ascii_lowercase(),
            value: value.as_bytes().to_vec(),
        });
    }
    Ok(headers)
}

#[derive(Clone, Copy)]
enum TransferDirection {
    Request,
    Response,
}

struct BufferedStream<'a> {
    stream: &'a mut TcpStream,
    buffer: Vec<u8>,
    cursor: usize,
    idle_timeout: Duration,
    direction: TransferDirection,
}

impl<'a> BufferedStream<'a> {
    fn new(
        stream: &'a mut TcpStream,
        buffer: Vec<u8>,
        idle_timeout: Duration,
        direction: TransferDirection,
    ) -> Self {
        Self {
            stream,
            buffer,
            cursor: 0,
            idle_timeout,
            direction,
        }
    }

    fn timeout_error(&self) -> EgressError {
        match self.direction {
            TransferDirection::Request => EgressError::BodyTimeout,
            TransferDirection::Response => EgressError::ResponseTimeout,
        }
    }

    fn eof_error(&self) -> EgressError {
        match self.direction {
            TransferDirection::Request => EgressError::UnexpectedEof,
            TransferDirection::Response => EgressError::UnexpectedUpstreamEof,
        }
    }

    fn remaining(&self) -> &[u8] {
        &self.buffer[self.cursor..]
    }

    fn has_buffered_bytes(&self) -> bool {
        self.cursor != self.buffer.len()
    }

    async fn read_more(&mut self) -> Result<(), EgressError> {
        self.read_more_with_timeout(self.idle_timeout).await
    }

    async fn read_more_with_timeout(&mut self, read_timeout: Duration) -> Result<(), EgressError> {
        if self.cursor == self.buffer.len() {
            self.buffer.clear();
            self.cursor = 0;
        } else if self.cursor > 4_096 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
        let mut bytes = [0_u8; 4_096];
        let count = timeout(read_timeout, self.stream.read(&mut bytes))
            .await
            .map_err(|_| self.timeout_error())??;
        if count == 0 {
            return Err(self.eof_error());
        }
        self.buffer.extend_from_slice(&bytes[..count]);
        Ok(())
    }

    async fn read_http_head(
        &mut self,
        limit: usize,
        header_timeout: Duration,
    ) -> Result<Vec<u8>, EgressError> {
        loop {
            if let Some(end) = find_header_end(self.remaining()) {
                if end > limit {
                    return Err(EgressError::ResponseHeaderTooLarge(limit));
                }
                let head = self.remaining()[..end].to_vec();
                self.cursor += end;
                return Ok(head);
            }
            if self.remaining().len() > limit {
                return Err(EgressError::ResponseHeaderTooLarge(limit));
            }
            self.read_more_with_timeout(header_timeout).await?;
        }
    }

    async fn read_crlf_line(&mut self, limit: usize) -> Result<Vec<u8>, EgressError> {
        loop {
            if let Some(index) = self
                .remaining()
                .windows(2)
                .position(|window| window == b"\r\n")
            {
                let length = index + 2;
                if length > limit {
                    return Err(EgressError::ChunkMetadataTooLarge(limit));
                }
                let line = self.remaining()[..length].to_vec();
                self.cursor += length;
                return Ok(line);
            }
            if self.remaining().len() >= limit {
                return Err(EgressError::ChunkMetadataTooLarge(limit));
            }
            self.read_more().await?;
        }
    }

    async fn copy_exact_to(
        &mut self,
        output: &mut TcpStream,
        mut length: u64,
    ) -> Result<(), EgressError> {
        if !self.remaining().is_empty() && length != 0 {
            let available = u64::try_from(self.remaining().len()).unwrap_or(u64::MAX);
            let count = usize::try_from(available.min(length)).unwrap_or(self.remaining().len());
            self.write_to(output, &self.remaining()[..count]).await?;
            self.cursor += count;
            length -= u64::try_from(count).unwrap_or(u64::MAX);
        }
        let mut bytes = [0_u8; 16 * 1_024];
        while length != 0 {
            let count = usize::try_from(length.min(bytes.len() as u64)).unwrap_or(bytes.len());
            let read = timeout(self.idle_timeout, self.stream.read(&mut bytes[..count]))
                .await
                .map_err(|_| self.timeout_error())??;
            if read == 0 {
                return Err(self.eof_error());
            }
            self.write_to(output, &bytes[..read]).await?;
            length -= u64::try_from(read).unwrap_or(u64::MAX);
        }
        Ok(())
    }

    async fn copy_until_eof_to(&mut self, output: &mut TcpStream) -> Result<(), EgressError> {
        if !self.remaining().is_empty() {
            let bytes = self.remaining().to_vec();
            self.cursor = self.buffer.len();
            self.write_to(output, &bytes).await?;
        }
        let mut bytes = [0_u8; 16 * 1_024];
        loop {
            let count = timeout(self.idle_timeout, self.stream.read(&mut bytes))
                .await
                .map_err(|_| self.timeout_error())??;
            if count == 0 {
                return Ok(());
            }
            self.write_to(output, &bytes[..count]).await?;
        }
    }

    async fn write_to(&self, output: &mut TcpStream, bytes: &[u8]) -> Result<(), EgressError> {
        timeout(self.idle_timeout, output.write_all(bytes))
            .await
            .map_err(|_| self.timeout_error())??;
        Ok(())
    }
}

async fn forward_one_request(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    head: &[u8],
    framing: MessageFraming,
    remainder: Vec<u8>,
    config: &EgressConfig,
) -> Result<(), EgressError> {
    write_all_with_timeout(upstream, head, config.idle_timeout).await?;
    let mut input = BufferedStream::new(
        client,
        remainder,
        config.idle_timeout,
        TransferDirection::Request,
    );
    match framing {
        MessageFraming::None => {}
        MessageFraming::ContentLength(length) => {
            Box::pin(input.copy_exact_to(upstream, length)).await?;
        }
        MessageFraming::Chunked => {
            Box::pin(forward_chunked_body(
                &mut input,
                upstream,
                config.max_header_bytes,
            ))
            .await?;
        }
    }
    if input.has_buffered_bytes() {
        return Err(EgressError::MalformedRequest(
            "pipelined requests are forbidden".to_owned(),
        ));
    }
    timeout(config.idle_timeout, upstream.shutdown())
        .await
        .map_err(|_| EgressError::BodyTimeout)??;
    Ok(())
}

async fn forward_chunked_body(
    input: &mut BufferedStream<'_>,
    output: &mut TcpStream,
    metadata_limit: usize,
) -> Result<(), EgressError> {
    let mut metadata_bytes = 0_usize;
    loop {
        let line = input
            .read_crlf_line(metadata_limit.saturating_sub(metadata_bytes))
            .await?;
        metadata_bytes = metadata_bytes.saturating_add(line.len());
        let chunk_size = parse_chunk_size(&line)?;
        input.write_to(output, &line).await?;
        if chunk_size == 0 {
            forward_chunk_trailers(input, output, metadata_limit, &mut metadata_bytes).await?;
            return Ok(());
        }
        Box::pin(input.copy_exact_to(output, chunk_size)).await?;
        let delimiter = input.read_crlf_line(2).await?;
        if delimiter != b"\r\n" {
            return Err(EgressError::MalformedRequest(
                "chunk data is not followed by CRLF".to_owned(),
            ));
        }
        input.write_to(output, &delimiter).await?;
    }
}

async fn forward_chunk_trailers(
    input: &mut BufferedStream<'_>,
    output: &mut TcpStream,
    metadata_limit: usize,
    metadata_bytes: &mut usize,
) -> Result<(), EgressError> {
    loop {
        let line = input
            .read_crlf_line(metadata_limit.saturating_sub(*metadata_bytes))
            .await?;
        *metadata_bytes = metadata_bytes.saturating_add(line.len());
        if line == b"\r\n" {
            input.write_to(output, &line).await?;
            return Ok(());
        }
        validate_trailer_line(&line)?;
        input.write_to(output, &line).await?;
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<u64, EgressError> {
    let value = line.strip_suffix(b"\r\n").ok_or_else(|| {
        EgressError::MalformedRequest("chunk-size line is not CRLF terminated".to_owned())
    })?;
    if value.iter().any(|byte| !matches!(byte, 0x20..=0x7e)) {
        return Err(EgressError::MalformedRequest(
            "invalid byte in chunk-size line".to_owned(),
        ));
    }
    let (digits, extension) = value
        .iter()
        .position(|byte| *byte == b';')
        .map_or((value, None), |separator| {
            (&value[..separator], Some(&value[separator + 1..]))
        });
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_hexdigit)
        || extension.is_some_and(<[u8]>::is_empty)
    {
        return Err(EgressError::MalformedRequest(
            "invalid chunk size".to_owned(),
        ));
    }
    digits.iter().try_fold(0_u64, |size, byte| {
        let digit = u64::from(char::from(*byte).to_digit(16).unwrap_or_default());
        size.checked_mul(16)
            .and_then(|value| value.checked_add(digit))
            .ok_or_else(|| EgressError::MalformedRequest("chunk size overflow".to_owned()))
    })
}

fn validate_trailer_line(line: &[u8]) -> Result<(), EgressError> {
    let value = line.strip_suffix(b"\r\n").ok_or_else(|| {
        EgressError::MalformedRequest("trailer line is not CRLF terminated".to_owned())
    })?;
    let colon = value
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| EgressError::MalformedRequest("trailer field has no colon".to_owned()))?;
    let name = &value[..colon];
    if name.is_empty() || !name.iter().copied().all(is_http_token_byte) {
        return Err(EgressError::MalformedRequest(
            "invalid trailer field name".to_owned(),
        ));
    }
    if value[colon + 1..]
        .iter()
        .any(|byte| byte.is_ascii_control() && *byte != b'\t')
    {
        return Err(EgressError::MalformedRequest(
            "invalid trailer field value".to_owned(),
        ));
    }
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "host"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) {
        return Err(EgressError::MalformedRequest(format!(
            "forbidden trailer field {name}"
        )));
    }
    Ok(())
}

async fn write_all_with_timeout(
    stream: &mut TcpStream,
    bytes: &[u8],
    idle_timeout: Duration,
) -> Result<(), EgressError> {
    timeout(idle_timeout, stream.write_all(bytes))
        .await
        .map_err(|_| EgressError::BodyTimeout)??;
    Ok(())
}

async fn relay_one_response(
    upstream: &mut TcpStream,
    client: &mut TcpStream,
    request_method: &str,
    config: &EgressConfig,
) -> Result<(), EgressError> {
    let mut input = BufferedStream::new(
        upstream,
        Vec::new(),
        config.idle_timeout,
        TransferDirection::Response,
    );
    for _ in 0..9 {
        let head =
            Box::pin(input.read_http_head(config.max_header_bytes, config.header_timeout)).await?;
        let response = ParsedResponse::parse(&head)?;
        if response.status == 101 {
            return Err(EgressError::MalformedResponse(
                "protocol upgrades are forbidden for forwarded HTTP".to_owned(),
            ));
        }
        let final_response = !response.is_informational();
        input
            .write_to(client, &response.forward_head(final_response))
            .await?;
        if !final_response {
            continue;
        }
        match response.framing(request_method) {
            ResponseFraming::None => {}
            ResponseFraming::ContentLength(length) => {
                Box::pin(input.copy_exact_to(client, length)).await?;
            }
            ResponseFraming::Chunked => {
                Box::pin(forward_chunked_body(
                    &mut input,
                    client,
                    config.max_header_bytes,
                ))
                .await?;
            }
            ResponseFraming::CloseDelimited => {
                Box::pin(input.copy_until_eof_to(client)).await?;
            }
        }
        drain_available_client_input(client)?;
        client.shutdown().await?;
        return Ok(());
    }
    Err(EgressError::MalformedResponse(
        "too many informational responses".to_owned(),
    ))
}

fn drain_available_client_input(client: &TcpStream) -> Result<(), EgressError> {
    let mut discarded = [0_u8; 4_096];
    let mut remaining = MAX_DISCARDED_PIPELINE_BYTES;
    while remaining != 0 {
        let read_limit = remaining.min(discarded.len());
        match client.try_read(&mut discarded[..read_limit]) {
            Ok(0) => return Ok(()),
            Ok(count) => remaining -= count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(EgressError::Io(error)),
        }
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
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

fn validate_framing(headers: &[Header]) -> Result<MessageFraming, EgressError> {
    let content_lengths = headers
        .iter()
        .filter(|header| header.name == "content-length")
        .collect::<Vec<_>>();
    let transfer_encodings = headers
        .iter()
        .filter(|header| header.name == "transfer-encoding")
        .collect::<Vec<_>>();
    if content_lengths.len() > 1 || transfer_encodings.len() > 1 {
        return Err(EgressError::MalformedRequest(
            "duplicate message-framing header".to_owned(),
        ));
    }
    if !content_lengths.is_empty() && !transfer_encodings.is_empty() {
        return Err(EgressError::MalformedRequest(
            "both Content-Length and Transfer-Encoding are present".to_owned(),
        ));
    }
    if let Some(header) = content_lengths.first() {
        if header.value.is_empty() || !header.value.iter().all(u8::is_ascii_digit) {
            return Err(EgressError::MalformedRequest(
                "invalid Content-Length".to_owned(),
            ));
        }
        let length = std::str::from_utf8(&header.value)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| EgressError::MalformedRequest("Content-Length overflow".to_owned()))?;
        return Ok(MessageFraming::ContentLength(length));
    }
    if let Some(header) = transfer_encodings.first() {
        if !header.value.eq_ignore_ascii_case(b"chunked") {
            return Err(EgressError::MalformedRequest(
                "unsupported Transfer-Encoding".to_owned(),
            ));
        }
        return Ok(MessageFraming::Chunked);
    }
    Ok(MessageFraming::None)
}

fn unique_header<'a>(headers: &'a [Header], name: &str) -> Result<Option<&'a str>, EgressError> {
    let mut matching = headers.iter().filter(|header| header.name == name);
    let first = matching.next();
    if matching.next().is_some() {
        return Err(EgressError::MalformedRequest(format!(
            "duplicate {name} header"
        )));
    }
    first
        .map(|header| {
            std::str::from_utf8(&header.value)
                .map_err(|_| EgressError::MalformedRequest(format!("non-UTF-8 {name} header")))
        })
        .transpose()
}

fn parse_request_target(
    method: &str,
    raw_target: &str,
    host_header: Option<&str>,
) -> Result<RequestTarget, EgressError> {
    if method.eq_ignore_ascii_case("CONNECT") {
        let authority = Authority::parse(raw_target, None)?;
        if let Some(host) = host_header {
            let host = Authority::parse(host, Some(authority.port))?;
            if host != authority {
                return Err(EgressError::MalformedRequest(
                    "CONNECT target and Host header disagree".to_owned(),
                ));
            }
        }
        return Ok(RequestTarget::Connect(authority));
    }

    if raw_target.starts_with("http://") || raw_target.starts_with("https://") {
        let url = Url::parse(raw_target).map_err(|error| {
            EgressError::MalformedRequest(format!("invalid proxy URL: {error}"))
        })?;
        if url.scheme() != "http" {
            return Err(EgressError::MalformedRequest(
                "absolute-form forwarding supports only http; use CONNECT for TLS".to_owned(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(EgressError::MalformedRequest(
                "credentials and fragments are forbidden in proxy URLs".to_owned(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| EgressError::MalformedRequest("proxy URL has no host".to_owned()))?;
        let authority = Authority {
            host: NormalizedHost::parse(host)?,
            port: url.port_or_known_default().ok_or_else(|| {
                EgressError::MalformedRequest("proxy URL has no target port".to_owned())
            })?,
        };
        if let Some(host) = host_header {
            let host = Authority::parse(host, Some(80))?;
            if host != authority {
                return Err(EgressError::MalformedRequest(
                    "proxy URL and Host header disagree".to_owned(),
                ));
            }
        } else {
            return Err(EgressError::MalformedRequest(
                "HTTP request is missing Host header".to_owned(),
            ));
        }
        let mut path = url.path().to_owned();
        if path.is_empty() {
            path.push('/');
        }
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        return Ok(RequestTarget::Forward { authority, path });
    }

    if !raw_target.starts_with('/') && raw_target != "*" {
        return Err(EgressError::MalformedRequest(
            "request target must be absolute-form or origin-form".to_owned(),
        ));
    }
    let host = host_header.ok_or_else(|| {
        EgressError::MalformedRequest("HTTP request is missing Host header".to_owned())
    })?;
    Ok(RequestTarget::Forward {
        authority: Authority::parse(host, Some(80))?,
        path: raw_target.to_owned(),
    })
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Resolves an authority once and validates the complete answer against SSRF policy.
///
/// # Errors
///
/// Returns an error when DNS fails, has no answers, or resolves to an address
/// denied by policy.
pub async fn resolve_checked(
    authority: &Authority,
    address_policy: &AddressPolicy,
) -> Result<Vec<SocketAddr>, EgressError> {
    let mut addresses = match authority.host {
        NormalizedHost::Ip(ip) => vec![SocketAddr::new(ip, authority.port)],
        NormalizedHost::Domain(ref domain) => lookup_host((domain.as_str(), authority.port))
            .await
            .map_err(|source| EgressError::Resolve {
                target: authority.clone(),
                source,
            })?
            .collect::<Vec<_>>(),
    };
    addresses.sort_unstable();
    addresses.dedup();
    address_policy.validate(authority, &addresses)?;
    Ok(addresses)
}

/// Resolves, checks, and connects without performing a second hostname lookup.
///
/// # Errors
///
/// Returns an error when resolution/policy validation fails, the timeout
/// expires, or none of the resolved addresses accepts a connection.
pub async fn connect_checked(
    authority: &Authority,
    address_policy: &AddressPolicy,
    connect_timeout: Duration,
) -> Result<TcpStream, EgressError> {
    timeout(connect_timeout, async {
        let addresses = resolve_checked(authority, address_policy).await?;
        TcpStream::connect(addresses.as_slice())
            .await
            .map_err(|source| EgressError::Connect {
                target: authority.clone(),
                source,
            })
    })
    .await
    .map_err(|_| EgressError::ConnectTimeout(authority.clone()))?
}

async fn write_connect_established(stream: &mut TcpStream) -> Result<(), EgressError> {
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .map_err(EgressError::Io)
}

async fn send_proxy_error(stream: &mut TcpStream, error: &EgressError) -> Result<(), EgressError> {
    let (status, reason, message, extra) = match error {
        EgressError::ProxyAuthenticationRequired => (
            407,
            "Proxy Authentication Required",
            "proxy authentication required",
            vec![("Proxy-Authenticate", "Bearer")],
        ),
        EgressError::DestinationDenied(_) | EgressError::AddressPolicy(_) => (
            403,
            "Forbidden",
            "destination denied by egress policy",
            vec![],
        ),
        EgressError::HeaderTooLarge(_) => (
            431,
            "Request Header Fields Too Large",
            "request headers too large",
            vec![],
        ),
        EgressError::HeaderTimeout => (408, "Request Timeout", "request header timed out", vec![]),
        EgressError::BodyTimeout => (408, "Request Timeout", "request body timed out", vec![]),
        EgressError::ChunkMetadataTooLarge(_) => (
            400,
            "Bad Request",
            "chunk metadata exceeds the proxy limit",
            vec![],
        ),
        EgressError::MalformedRequest(_) | EgressError::UnexpectedEof => {
            (400, "Bad Request", "malformed proxy request", vec![])
        }
        EgressError::ConnectTimeout(_) => (504, "Gateway Timeout", "upstream timed out", vec![]),
        EgressError::Resolve { .. } | EgressError::Connect { .. } => {
            (502, "Bad Gateway", "upstream connection failed", vec![])
        }
        EgressError::ResponseTimeout => (
            504,
            "Gateway Timeout",
            "upstream response timed out",
            vec![],
        ),
        _ => (500, "Internal Server Error", "proxy error", vec![]),
    };
    write_response(stream, status, reason, message, &extra).await
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> Result<(), EgressError> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(body);
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Egress proxy parsing, policy, resolution, and transport errors.
#[derive(Debug, Error)]
pub enum EgressError {
    #[error("invalid egress configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to accept egress connection: {0}")]
    Accept(#[source] io::Error),
    #[error("proxy request header timed out")]
    HeaderTimeout,
    #[error("proxy request header exceeded {0} bytes")]
    HeaderTooLarge(usize),
    #[error("client closed before sending a complete request")]
    UnexpectedEof,
    #[error("request body transfer timed out")]
    BodyTimeout,
    #[error("chunk metadata exceeded {0} bytes")]
    ChunkMetadataTooLarge(usize),
    #[error("upstream response header exceeded {0} bytes")]
    ResponseHeaderTooLarge(usize),
    #[error("upstream closed before sending a complete response")]
    UnexpectedUpstreamEof,
    #[error("malformed upstream response: {0}")]
    MalformedResponse(String),
    #[error("malformed proxy request: {0}")]
    MalformedRequest(String),
    #[error("proxy authentication required")]
    ProxyAuthenticationRequired,
    #[error("destination denied by allow-list: {0}")]
    DestinationDenied(Authority),
    #[error("destination address denied: {0}")]
    AddressPolicy(#[from] AllowlistError),
    #[error("failed to resolve {target}: {source}")]
    Resolve {
        target: Authority,
        #[source]
        source: io::Error,
    },
    #[error("connection to {0} timed out")]
    ConnectTimeout(Authority),
    #[error("failed to connect to {target}: {source}")]
    Connect {
        target: Authority,
        #[source]
        source: io::Error,
    },
    #[error("egress relay failed: {0}")]
    Relay(#[source] RelayError),
    #[error("upstream response transfer timed out")]
    ResponseTimeout,
    #[error("egress I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn parse(input: &str) -> ParsedRequest {
        ParsedRequest::parse(input.as_bytes()).unwrap()
    }

    #[test]
    fn parses_and_rewrites_absolute_http_request() {
        let request = parse(
            "POST http://Example.com/api?q=1 HTTP/1.1\r\n\
             Host: example.com\r\n\
             Proxy-Authorization: Bearer secret\r\n\
             Connection: keep-alive, x-remove\r\n\
             X-Remove: yes\r\n\
             Content-Length: 3\r\n\r\n",
        );
        assert_eq!(request.authority().to_string(), "example.com:80");
        let RequestTarget::Forward { path, .. } = &request.target else {
            panic!("expected forward request");
        };
        let head = String::from_utf8(request.forward_head(path)).unwrap();
        assert!(head.starts_with("POST /api?q=1 HTTP/1.1\r\nHost: example.com:80\r\n"));
        assert!(head.contains("content-length: 3\r\n"));
        assert!(!head.contains("secret"));
        assert!(!head.contains("x-remove"));
        assert!(head.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn parses_connect_ipv6_and_checks_host() {
        let request =
            parse("CONNECT [2001:db8::1]:443 HTTP/1.1\r\nHost: [2001:db8::1]:443\r\n\r\n");
        assert_eq!(request.authority().to_string(), "[2001:db8::1]:443");
        assert!(
            ParsedRequest::parse(
                b"CONNECT example.com:443 HTTP/1.1\r\nHost: attacker.test:443\r\n\r\n"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_request_smuggling_shapes() {
        for request in [
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nTransfer-Encoding: gzip\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nTransfer-Encoding: chunked, chunked\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nContent-Length: 18446744073709551616\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nContent-Length: 1, 1\r\n\r\n",
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nExpect: 100-continue\r\nContent-Length: 1\r\n\r\n",
            "GET http://e.test/ HTTP/1.1\nHost: e.test\n\n",
            "GET http://e.test/ HTTP/1.1\r\nHost: a.test\r\nHost: a.test\r\n\r\n",
        ] {
            assert!(ParsedRequest::parse(request.as_bytes()).is_err());
        }
    }

    #[test]
    fn framing_is_explicit_and_rejects_buffered_pipeline_bytes() {
        let no_body = parse("GET http://e.test/ HTTP/1.1\r\nHost: e.test\r\n\r\n");
        assert_eq!(no_body.framing, MessageFraming::None);
        assert!(no_body.validate_buffered_prefix(b"GET /second").is_err());

        let fixed =
            parse("POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nContent-Length: 4\r\n\r\n");
        assert_eq!(fixed.framing, MessageFraming::ContentLength(4));
        assert!(fixed.validate_buffered_prefix(b"BODY").is_ok());
        assert!(fixed.validate_buffered_prefix(b"BODYG").is_err());

        let chunked = parse(
            "POST http://e.test/ HTTP/1.1\r\nHost: e.test\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        assert_eq!(chunked.framing, MessageFraming::Chunked);
    }

    #[test]
    fn chunk_sizes_and_trailers_are_strict() {
        assert_eq!(parse_chunk_size(b"10\r\n").unwrap(), 16);
        assert_eq!(parse_chunk_size(b"a;name=value\r\n").unwrap(), 10);
        for line in [
            &b"\r\n"[..],
            b"+1\r\n",
            b"0x1\r\n",
            b"1;\r\n",
            b"10000000000000000\r\n",
            b"1\n",
        ] {
            assert!(parse_chunk_size(line).is_err(), "accepted {line:?}");
        }
        assert!(validate_trailer_line(b"X-Checksum: yes\r\n").is_ok());
        for line in [
            &b"Host: attacker.test\r\n"[..],
            b"Content-Length: 5\r\n",
            b"Transfer-Encoding: chunked\r\n",
            b"Broken\r\n",
            b" bad: value\r\n",
        ] {
            assert!(validate_trailer_line(line).is_err(), "accepted {line:?}");
        }
    }

    #[test]
    fn response_framing_is_strict_and_hop_headers_are_removed() {
        let response = ParsedResponse::parse(
            b"HTTP/1.1 200 OK\r\n\
              Content-Length: 2\r\n\
              Connection: keep-alive, x-remove\r\n\
              X-Remove: secret\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.framing("GET"), ResponseFraming::ContentLength(2));
        let forwarded = String::from_utf8(response.forward_head(true)).unwrap();
        assert!(forwarded.contains("content-length: 2\r\n"));
        assert!(!forwarded.contains("x-remove"));
        assert!(forwarded.ends_with("Connection: close\r\n\r\n"));

        for head in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\nContent-Length: 1\n\n",
            b"HTTP/1.1 204 No Content\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(ParsedResponse::parse(head).is_err(), "accepted {head:?}");
        }
    }

    #[test]
    fn bearer_auth_is_exact_and_redacted_from_forwarding() {
        let token = AuthToken::new(b"secret".to_vec()).unwrap();
        let correct = parse(
            "GET http://e.test/ HTTP/1.1\r\nHost: e.test\r\nProxy-Authorization: Bearer secret\r\n\r\n",
        );
        let wrong = parse(
            "GET http://e.test/ HTTP/1.1\r\nHost: e.test\r\nProxy-Authorization: Bearer secret2\r\n\r\n",
        );
        assert!(correct.is_authorized(Some(&token)));
        assert!(!wrong.is_authorized(Some(&token)));
    }

    #[tokio::test]
    async fn health_endpoint_and_denial_work_over_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            serve_egress(listener, EgressConfig::default(), async {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut health = TcpStream::connect(address).await.unwrap();
        health
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        health.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200 OK"));

        let mut denied = TcpStream::connect(address).await.unwrap();
        denied
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        denied.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 403 Forbidden"));

        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn full_connect_tunnel_reaches_explicit_private_service() {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_address = destination.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = destination.accept().await.unwrap();
            let mut data = Vec::new();
            stream.read_to_end(&mut data).await.unwrap();
            stream.write_all(&data).await.unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let rule = destination_address.to_string();
        let config = EgressConfig {
            allowlist: AllowList::parse([rule.as_str()]).unwrap(),
            address_policy: AddressPolicy {
                private_authorities: AllowList::parse([rule.as_str()]).unwrap(),
            },
            ..EgressConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            serve_egress(listener, config, async {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut client = TcpStream::connect(proxy_address).await.unwrap();
        client
            .write_all(
                format!(
                    "CONNECT {destination_address} HTTP/1.1\r\nHost: {destination_address}\r\n\r\npayload"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut head = vec![0_u8; 39];
        client.read_exact(&mut head).await.unwrap();
        assert_eq!(&head, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        client.shutdown().await.unwrap();
        let mut echoed = Vec::new();
        client.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"payload");

        echo.await.unwrap();
        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }

    async fn start_capturing_upstream() -> (
        SocketAddr,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<Vec<u8>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            let _ = captured_tx.send(request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
        });
        (address, accepted_rx, captured_rx, task)
    }

    async fn start_test_proxy(
        destination: SocketAddr,
    ) -> (
        SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), EgressError>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let rule = destination.to_string();
        let config = EgressConfig {
            allowlist: AllowList::parse([rule.as_str()]).unwrap(),
            address_policy: AddressPolicy {
                private_authorities: AllowList::parse([rule.as_str()]).unwrap(),
            },
            idle_timeout: Duration::from_secs(2),
            ..EgressConfig::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            serve_egress(listener, config, async {
                let _ = shutdown_rx.await;
            })
            .await
        });
        (address, shutdown_tx, task)
    }

    #[tokio::test]
    async fn content_length_pipeline_is_not_forwarded_and_client_need_not_half_close() {
        let (destination, accepted, captured, upstream) = start_capturing_upstream().await;
        let (proxy, shutdown, server) = start_test_proxy(destination).await;
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(
                format!(
                    "POST http://{destination}/first HTTP/1.1\r\n\
                     Host: {destination}\r\nContent-Length: 4\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), accepted)
            .await
            .unwrap()
            .unwrap();
        client
            .write_all(b"BODYGET http://attacker.test/ HTTP/1.1\r\nHost: attacker.test\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
        let request = timeout(Duration::from_secs(1), captured)
            .await
            .unwrap()
            .unwrap();
        assert!(request.ends_with(b"\r\n\r\nBODY"));
        assert!(!request.windows(13).any(|bytes| bytes == b"attacker.test"));

        upstream.await.unwrap();
        let _ = shutdown.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn chunked_pipeline_is_rejected_without_forwarding_second_request() {
        let (destination, accepted, captured, upstream) = start_capturing_upstream().await;
        let (proxy, shutdown, server) = start_test_proxy(destination).await;
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(
                format!(
                    "POST http://{destination}/first HTTP/1.1\r\n\
                     Host: {destination}\r\nTransfer-Encoding: chunked\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), accepted)
            .await
            .unwrap()
            .unwrap();
        client
            .write_all(
                b"4\r\nBODY\r\n0\r\n\r\nGET http://attacker.test/ HTTP/1.1\r\nHost: attacker.test\r\n\r\n",
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
        let request = timeout(Duration::from_secs(1), captured)
            .await
            .unwrap()
            .unwrap();
        assert!(request.ends_with(b"4\r\nBODY\r\n0\r\n\r\n"));
        assert!(!request.windows(13).any(|bytes| bytes == b"attacker.test"));

        upstream.await.unwrap();
        let _ = shutdown.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bytes_after_framed_upstream_response_are_not_relayed() {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_address = destination.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = destination.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            assert!(request.starts_with(b"GET /first HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK\
                      HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nEVIL",
                )
                .await
                .unwrap();
        });
        let (proxy, shutdown, server) = start_test_proxy(destination_address).await;
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(
                format!(
                    "GET http://{destination_address}/first HTTP/1.1\r\n\
                     Host: {destination_address}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(
            response,
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nConnection: close\r\n\r\nOK"
        );

        upstream.await.unwrap();
        let _ = shutdown.send(());
        server.await.unwrap().unwrap();
    }

    #[test]
    fn literal_private_address_is_checked_without_dns() {
        let authority = Authority::parse("127.0.0.1:80", None).unwrap();
        assert!(matches!(
            AddressPolicy::default().validate(
                &authority,
                &[SocketAddr::new(
                    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    80
                )]
            ),
            Err(AllowlistError::PrivateAddressDenied { .. })
        ));
    }
}
