//! User-facing host orchestration errors.

use std::{ffi::OsString, io, path::PathBuf, process::ExitStatus};

use thiserror::Error;

/// Errors produced by the codex-start host process.
#[derive(Debug, Error)]
pub enum HostError {
    /// A command failed to start or complete.
    #[error("could not run {program:?}: {source}")]
    CommandIo {
        /// Executable name.
        program: OsString,
        /// Underlying operating-system error.
        source: io::Error,
    },
    /// An external command returned a non-zero status.
    #[error("{program:?} failed with {status}: {stderr}")]
    CommandFailed {
        /// Executable name.
        program: OsString,
        /// Exit status.
        status: ExitStatus,
        /// Sanitized stderr.
        stderr: String,
    },
    /// An expected executable was unavailable.
    #[error("required executable is not available: {0}")]
    ExecutableMissing(String),
    /// Configuration could not be loaded or validated.
    #[error("configuration error: {0}")]
    Config(String),
    /// A runtime operation was invalid or unavailable.
    #[error("container runtime error: {0}")]
    Runtime(String),
    /// A Git operation was invalid or failed.
    #[error("Git error: {0}")]
    Git(String),
    /// A requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// A path was unsafe or incompatible with the request.
    #[error("unsafe path {path}: {reason}")]
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
        /// Reason for rejection.
        reason: String,
    },
    /// A regular I/O operation failed.
    #[error("I/O error for {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// Serialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// The user supplied an invalid command line.
    #[error("{0}")]
    Usage(String),
}

impl HostError {
    /// Stable process exit code category.
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Config(_) => 2,
            Self::Runtime(_) | Self::ExecutableMissing(_) => 3,
            Self::Git(_) | Self::UnsafePath { .. } => 4,
            _ => 1,
        }
    }

    /// Attach a filesystem path to an I/O error.
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Convenient result alias for host operations.
pub type Result<T> = std::result::Result<T, HostError>;
