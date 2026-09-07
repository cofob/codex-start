//! Direct and embedded Yggdrasil transports with a common stream interface.
pub mod overlay;
pub mod peers;
mod stack;
pub mod tls;

pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}
pub type Stream = Box<dyn AsyncStream>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("transport I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Remote(#[from] codex_start_remote::Error),
}
