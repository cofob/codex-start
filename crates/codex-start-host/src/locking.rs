//! Advisory locks preventing concurrent mutation of the same runtime identity.

use std::{
    fs,
    path::{Path, PathBuf},
};

use fs2::FileExt;

use crate::{
    error::{HostError, Result},
    paths::create_private_dir,
};

/// Exclusive lock held for one workload/container identity.
#[derive(Debug)]
pub struct RunLock {
    file: fs::File,
    path: PathBuf,
}

impl RunLock {
    /// Acquire a non-blocking per-name lock below the private runtime directory.
    pub fn acquire(runtime_dir: &Path, name: &str) -> Result<Self> {
        let directory = runtime_dir.join("locks");
        create_private_dir(&directory)?;
        let digest = blake3::hash(name.as_bytes()).to_hex();
        let path = directory.join(format!("{}-{}.lock", safe_prefix(name), &digest[..12]));
        let file = fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| HostError::io(&path, source))?;
        file.try_lock_exclusive().map_err(|source| {
            HostError::Runtime(format!(
                "another codex-start process is preparing or running {name:?}: {source}"
            ))
        })?;
        Ok(Self { file, path })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            tracing::warn!(path = %self.path.display(), %error, "failed to release run lock");
        }
    }
}

fn safe_prefix(value: &str) -> String {
    let prefix = value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        .take(32)
        .map(char::from)
        .collect::<String>();
    if prefix.is_empty() {
        "run".to_owned()
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::RunLock;

    #[test]
    fn same_run_identity_cannot_be_locked_twice() {
        let directory = tempfile::tempdir().expect("runtime");
        let first = RunLock::acquire(directory.path(), "codex-project").expect("first");
        assert!(RunLock::acquire(directory.path(), "codex-project").is_err());
        drop(first);
        assert!(RunLock::acquire(directory.path(), "codex-project").is_ok());
    }
}
