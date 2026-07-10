//! Self-contained, content-addressed Docker build assets.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use fs2::FileExt;

use crate::{
    error::{HostError, Result},
    paths::{AppPaths, atomic_write, create_private_dir},
};

include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

const MARKER: &str = ".codex-start-assets";

/// Materialize the build bundle compiled into the executable.
///
/// The versioned directory is immutable after validation. An advisory lock
/// serializes first-use extraction by concurrent launcher processes.
pub fn materialize(paths: &AppPaths) -> Result<PathBuf> {
    let digest = bundle_digest();
    let parent = paths.cache.join("build-assets");
    create_private_dir(&parent)?;
    let lock_path = parent.join(".lock");
    let lock = fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| HostError::io(&lock_path, source))?;
    lock.lock_exclusive()
        .map_err(|source| HostError::io(&lock_path, source))?;

    let root = parent.join(format!("v{}-{}", env!("CARGO_PKG_VERSION"), &digest[..16]));
    let marker = root.join(MARKER);
    if fs::read_to_string(&marker).ok().as_deref() != Some(&digest) {
        create_private_dir(&root)?;
        for (relative, contents) in EMBEDDED_FILES {
            let relative = validated_relative(relative)?;
            let destination = root.join(relative);
            if fs::read(&destination).ok().as_deref() == Some(*contents) {
                continue;
            }
            atomic_write_bytes(&destination, contents)?;
        }
        atomic_write(&marker, &digest)?;
    }
    FileExt::unlock(&lock).map_err(|source| HostError::io(&lock_path, source))?;
    Ok(root)
}

fn bundle_digest() -> String {
    let mut hasher = blake3::Hasher::new();
    for (relative, contents) in EMBEDDED_FILES {
        hasher.update(relative.as_bytes());
        hasher.update(&[0]);
        hasher.update(contents);
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn validated_relative(value: &str) -> Result<&Path> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HostError::UnsafePath {
            path: path.to_path_buf(),
            reason: "embedded asset path is not a safe relative path".to_owned(),
        });
    }
    Ok(path)
}

fn atomic_write_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| HostError::UnsafePath {
        path: path.to_path_buf(),
        reason: "asset path has no parent".to_owned(),
    })?;
    fs::create_dir_all(parent).map_err(|source| HostError::io(parent, source))?;
    let temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| HostError::io(parent, source))?;
    fs::write(temporary.path(), contents)
        .map_err(|source| HostError::io(temporary.path(), source))?;
    temporary
        .persist(path)
        .map_err(|error| HostError::io(path, error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{bundle_digest, validated_relative};

    #[test]
    fn bundle_is_non_empty_and_paths_are_confined() {
        assert!(!bundle_digest().is_empty());
        assert!(validated_relative("images/environment/Dockerfile").is_ok());
        assert!(validated_relative("../escape").is_err());
        assert!(validated_relative("/absolute").is_err());
    }
}
