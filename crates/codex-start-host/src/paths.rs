//! XDG-compatible host path resolution and safe file helpers.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

use crate::error::{HostError, Result};

/// All persistent and ephemeral application roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    /// User-editable configuration root.
    pub config: PathBuf,
    /// Persistent mutable state root.
    pub data: PathBuf,
    /// Rebuildable cache and runtime root.
    pub cache: PathBuf,
}

impl AppPaths {
    /// Resolve paths from XDG variables, with portable home-directory defaults.
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| HostError::Config("HOME is not set".to_owned()))?;
        let config = env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join("codex-start");
        let data = env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("codex-start");
        let cache = env::var_os("XDG_CACHE_HOME")
            .map_or_else(|| home.join(".cache"), PathBuf::from)
            .join("codex-start");
        Ok(Self {
            config,
            data,
            cache,
        })
    }

    /// Ensure application roots exist with user-only permissions on Unix.
    pub fn ensure(&self) -> Result<()> {
        for path in [&self.config, &self.data, &self.cache] {
            create_private_dir(path)?;
        }
        for path in [
            self.environments_dir(),
            self.projects_dir(),
            self.homes_dir(),
            self.worktrees_dir(),
            self.sessions_dir(),
            self.runtime_dir(),
        ] {
            create_private_dir(&path)?;
        }
        Ok(())
    }

    /// Global settings file.
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// User environment manifests.
    pub fn environments_dir(&self) -> PathBuf {
        self.config.join("environments")
    }

    /// Non-Git project settings registry.
    pub fn projects_dir(&self) -> PathBuf {
        self.config.join("projects")
    }

    /// Managed user homes.
    pub fn homes_dir(&self) -> PathBuf {
        self.data.join("homes")
    }

    /// Linked worktree storage.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.data.join("worktrees")
    }

    /// Persistent session metadata and private launch bundles.
    pub fn sessions_dir(&self) -> PathBuf {
        self.data.join("sessions")
    }

    /// Generated secrets, sockets, and lifecycle state.
    pub fn runtime_dir(&self) -> PathBuf {
        self.cache.join("runtime")
    }

    /// Project configuration path for a non-Git canonical root.
    pub fn non_git_project_file(&self, canonical_root: &Path) -> PathBuf {
        self.projects_dir().join(format!(
            "{}.toml",
            codex_start_core::canonical_path_hash(canonical_root)
        ))
    }
}

/// Atomically replace a text file and restrict its permissions.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| HostError::UnsafePath {
        path: path.to_path_buf(),
        reason: "file has no parent directory".to_owned(),
    })?;
    create_private_dir(parent)?;
    ensure_regular_file_or_missing(path)?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| HostError::io(parent, source))?;
    temporary
        .write_all(contents.as_bytes())
        .map_err(|source| HostError::io(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| HostError::io(temporary.path(), source))?;
    set_private_file(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|error| HostError::io(path, error.error))?;
    Ok(())
}

/// Create a directory and restrict access to the current user where supported.
pub fn create_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(HostError::UnsafePath {
                path: path.to_path_buf(),
                reason: "expected a directory that is not a symbolic link".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(HostError::io(path, source)),
    }
    fs::create_dir_all(path).map_err(|source| HostError::io(path, source))?;
    let metadata = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HostError::UnsafePath {
            path: path.to_path_buf(),
            reason: "directory creation resolved to an unsafe file type".to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| HostError::io(path, source))?;
    }
    Ok(())
}

/// Restrict an existing file to the current user.
pub fn set_private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HostError::UnsafePath {
            path: path.to_path_buf(),
            reason: "expected a regular file that is not a symbolic link".to_owned(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|source| HostError::io(path, source))?;
    }
    Ok(())
}

/// Reject an existing symlink or non-regular target and report whether a file exists.
pub fn ensure_regular_file_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(HostError::UnsafePath {
                path: path.to_path_buf(),
                reason: "expected a regular file that is not a symbolic link".to_owned(),
            })
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(HostError::io(path, source)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{atomic_write, create_private_dir};

    #[test]
    fn atomic_write_replaces_complete_contents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = directory.path().join("nested/config.toml");
        atomic_write(&file, "first").expect("first write");
        atomic_write(&file, "second").expect("second write");
        assert_eq!(fs::read_to_string(file).expect("read"), "second");
    }

    #[cfg(unix)]
    #[test]
    fn private_paths_reject_symbolic_link_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        let directory_link = directory.path().join("directory-link");
        symlink(&outside, &directory_link).expect("directory symlink");
        assert!(create_private_dir(&directory_link).is_err());

        let outside_file = outside.join("config.toml");
        fs::write(&outside_file, "unchanged").expect("outside file");
        let file_link = directory.path().join("config.toml");
        symlink(&outside_file, &file_link).expect("file symlink");
        assert!(atomic_write(&file_link, "replacement").is_err());
        assert_eq!(
            fs::read_to_string(outside_file).expect("outside remains readable"),
            "unchanged"
        );
    }
}
