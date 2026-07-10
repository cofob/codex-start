//! Secret token handling shared by proxy authentication protocols.

use std::{fmt, io, path::Path};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

/// Maximum supported authentication-token size.
pub const MAX_TOKEN_BYTES: usize = 4_096;

/// An authentication token whose memory is cleared when dropped.
#[derive(Clone)]
pub struct AuthToken(Zeroizing<Vec<u8>>);

impl AuthToken {
    /// Creates a validated token.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty or exceeds the protocol limit.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, AuthTokenError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(AuthTokenError::Empty);
        }
        if bytes.len() > MAX_TOKEN_BYTES {
            return Err(AuthTokenError::TooLong {
                length: bytes.len(),
                maximum: MAX_TOKEN_BYTES,
            });
        }
        Ok(Self(Zeroizing::new(bytes)))
    }

    /// Loads a token from a file, removing one conventional trailing newline.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or its contents are not a
    /// valid token.
    pub fn from_file(path: &Path) -> Result<Self, AuthTokenError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::symlink_metadata(path).map_err(|source| AuthTokenError::Read {
            path: path.to_owned(),
            source,
        })?;
        if !metadata.file_type().is_file() {
            return Err(AuthTokenError::NotRegular(path.to_owned()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(AuthTokenError::InsecurePermissions {
                path: path.to_owned(),
                mode: metadata.mode() & 0o777,
            });
        }
        if metadata.len() > (MAX_TOKEN_BYTES + 2) as u64 {
            return Err(AuthTokenError::TooLong {
                length: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
                maximum: MAX_TOKEN_BYTES,
            });
        }
        let mut bytes = std::fs::read(path).map_err(|source| AuthTokenError::Read {
            path: path.to_owned(),
            source,
        })?;
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
        } else if bytes.ends_with(b"\n") {
            bytes.truncate(bytes.len() - 1);
        }
        Self::new(bytes)
    }

    /// Returns the token bytes for an authenticated client handshake.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Compares a candidate without leaking the matching prefix or token length.
    #[must_use]
    pub fn matches(&self, candidate: &[u8]) -> bool {
        let expected = Sha256::digest(&self.0);
        let actual = Sha256::digest(candidate);
        constant_time_eq(expected.as_slice(), actual.as_slice()) & (self.0.len() == candidate.len())
    }
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthToken([REDACTED])")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Errors returned while loading an authentication token.
#[derive(Debug, Error)]
pub enum AuthTokenError {
    /// Tokens must not be empty.
    #[error("authentication token is empty")]
    Empty,
    /// Tokens have a deliberately small protocol limit.
    #[error("authentication token is {length} bytes; maximum is {maximum}")]
    TooLong { length: usize, maximum: usize },
    /// The token file could not be read.
    #[error("failed to read authentication token {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
    /// Symlinks and special files are not accepted as token sources.
    #[error("authentication token {0} is not a regular file")]
    NotRegular(std::path::PathBuf),
    /// Token files must be private to their owner.
    #[error("authentication token {path} has insecure mode {mode:o}")]
    InsecurePermissions { path: std::path::PathBuf, mode: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_redacted_and_compared_exactly() {
        let token = AuthToken::new(b"correct horse".to_vec()).unwrap();
        assert!(token.matches(b"correct horse"));
        assert!(!token.matches(b"correct horses"));
        assert!(!token.matches(b"correct horsf"));
        assert_eq!(format!("{token:?}"), "AuthToken([REDACTED])");
    }

    #[test]
    fn file_loader_removes_one_newline() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, b"secret\r\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(AuthToken::from_file(&path).unwrap().matches(b"secret"));
    }

    #[test]
    fn file_loader_rejects_public_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            AuthToken::from_file(&path),
            Err(AuthTokenError::InsecurePermissions { .. })
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let linked = directory.path().join("linked");
        symlink(&path, &linked).unwrap();
        assert!(matches!(
            AuthToken::from_file(&linked),
            Err(AuthTokenError::NotRegular(_))
        ));
    }
}
