//! XDG-compatible host path resolution and safe file helpers.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

use codex_start_core::ContainerPath;

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
        #[cfg(unix)]
        {
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
        #[cfg(windows)]
        {
            let profile = env::var_os("USERPROFILE").map(PathBuf::from);
            let config = env::var_os("APPDATA")
                .map(PathBuf::from)
                .or_else(|| profile.as_ref().map(|path| path.join("AppData/Roaming")))
                .ok_or_else(|| {
                    HostError::Config(
                        "APPDATA and USERPROFILE are not set; cannot locate configuration"
                            .to_owned(),
                    )
                })?
                .join("codex-start");
            let data = env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .or_else(|| profile.as_ref().map(|path| path.join("AppData/Local")))
                .ok_or_else(|| {
                    HostError::Config(
                        "LOCALAPPDATA and USERPROFILE are not set; cannot locate application data"
                            .to_owned(),
                    )
                })?
                .join("codex-start");
            let cache = data.join("cache");
            Ok(Self {
                config,
                data,
                cache,
            })
        }
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
    #[cfg(windows)]
    restrict_windows_acl(path, true)?;
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
    #[cfg(windows)]
    restrict_windows_acl(path, false)?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path, directory: bool) -> Result<()> {
    let identity = std::process::Command::new("whoami")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let user = env::var("USERNAME").ok()?;
            env::var("USERDOMAIN")
                .ok()
                .filter(|domain| !domain.is_empty())
                .map_or_else(
                    || Some(user.clone()),
                    |domain| Some(format!("{domain}\\{user}")),
                )
        })
        .ok_or_else(|| {
            HostError::Config("could not determine the current Windows identity".to_owned())
        })?;
    let grant = if directory {
        format!("{identity}:(OI)(CI)F")
    } else {
        format!("{identity}:F")
    };
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(grant)
        .output()
        .map_err(|source| HostError::CommandIo {
            program: "icacls.exe".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(HostError::Runtime(format!(
            "could not restrict Windows ACL for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
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

/// Append trusted container path components with POSIX separators on every host.
pub fn join_container_components<'a>(
    base: &Path,
    components: impl IntoIterator<Item = &'a str>,
) -> Result<PathBuf> {
    let mut path = ContainerPath::new(base).map_err(|error| {
        HostError::Config(format!(
            "invalid container path {}: {error}",
            base.display()
        ))
    })?;
    for component in components {
        path = path.join_component(component).map_err(|error| {
            HostError::Config(format!(
                "invalid container path component {component:?}: {error}"
            ))
        })?;
    }
    Ok(path.into_path_buf())
}

/// Append a host-relative path to a container path using POSIX separators.
pub fn join_container_relative(base: &Path, relative: &Path) -> Result<PathBuf> {
    ContainerPath::new(base)
        .and_then(|path| path.join_relative(relative))
        .map(ContainerPath::into_path_buf)
        .map_err(|error| {
            HostError::Config(format!(
                "cannot append {} to container path {}: {error}",
                relative.display(),
                base.display()
            ))
        })
}

/// Return the POSIX parent of a validated non-root container path.
pub fn container_parent(path: &Path) -> Result<PathBuf> {
    let path = ContainerPath::new(path).map_err(|error| {
        HostError::Config(format!(
            "invalid container path {}: {error}",
            path.display()
        ))
    })?;
    let (parent, _) = path
        .as_str()
        .rsplit_once('/')
        .ok_or_else(|| HostError::Config(format!("container path {path} has no parent")))?;
    let parent = if parent.is_empty() { "/" } else { parent };
    ContainerPath::parse(parent)
        .map(ContainerPath::into_path_buf)
        .map_err(|error| HostError::Config(format!("invalid container parent {parent}: {error}")))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::atomic_write;
    #[cfg(unix)]
    use super::create_private_dir;

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
