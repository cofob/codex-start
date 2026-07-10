//! Stable project identity and XDG-compliant configuration locations.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(not(unix))]
use std::path::Component;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Whether identity is anchored by Git metadata or a canonical directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Git,
    Directory,
}

/// Stable identity shared by linked worktrees of the same repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdentity {
    pub kind: ProjectKind,
    /// Short BLAKE3 identifier used in labels, volumes, and container paths.
    pub id: String,
    /// Safe, recognizable project name.
    pub display_name: String,
    /// Root of the worktree or non-Git project that will be mounted.
    pub root: PathBuf,
    /// Canonical path used to produce `id` (Git common dir for repositories).
    pub identity_path: PathBuf,
    /// Canonical invocation directory.
    pub original_cwd: PathBuf,
    /// Invocation directory relative to `root`, preserved in the container.
    pub relative_workdir: PathBuf,
    pub git_common_dir: Option<PathBuf>,
}

impl ProjectIdentity {
    /// Construct a Git identity from paths already discovered by the host Git adapter.
    pub fn git(
        worktree_root: impl AsRef<Path>,
        git_common_dir: impl AsRef<Path>,
        original_cwd: impl AsRef<Path>,
    ) -> Result<Self, ProjectError> {
        let root = canonical_directory(worktree_root.as_ref(), "worktree root")?;
        let git_common_dir = canonical_directory(git_common_dir.as_ref(), "Git common directory")?;
        let original_cwd = canonical_directory(original_cwd.as_ref(), "working directory")?;
        let relative_workdir = relative_below(&original_cwd, &root)?;
        let project_name_path = if git_common_dir.file_name() == Some(OsStr::new(".git")) {
            git_common_dir.parent().unwrap_or(&root)
        } else {
            &git_common_dir
        };
        let display_name = safe_name_from_path(project_name_path);
        Ok(Self {
            kind: ProjectKind::Git,
            id: canonical_path_hash(&git_common_dir),
            display_name,
            root,
            identity_path: git_common_dir.clone(),
            original_cwd,
            relative_workdir,
            git_common_dir: Some(git_common_dir),
        })
    }

    /// Construct a non-Git identity anchored by the chosen project root.
    pub fn directory(
        root: impl AsRef<Path>,
        original_cwd: impl AsRef<Path>,
    ) -> Result<Self, ProjectError> {
        let root = canonical_directory(root.as_ref(), "project root")?;
        let original_cwd = canonical_directory(original_cwd.as_ref(), "working directory")?;
        let relative_workdir = relative_below(&original_cwd, &root)?;
        Ok(Self {
            kind: ProjectKind::Directory,
            id: canonical_path_hash(&root),
            display_name: safe_name_from_path(&root),
            root: root.clone(),
            identity_path: root,
            original_cwd,
            relative_workdir,
            git_common_dir: None,
        })
    }

    /// Per-project configuration location, shared by linked worktrees for Git repos.
    #[must_use]
    pub fn config_path(&self, app_paths: &AppPaths) -> PathBuf {
        self.git_common_dir.as_ref().map_or_else(
            || app_paths.projects.join(format!("{}.toml", self.id)),
            |common_dir| common_dir.join("codex-start.toml"),
        )
    }

    /// Unique workspace mount root for a named or current worktree.
    #[must_use]
    pub fn container_workspace(&self, worktree_name: &str) -> PathBuf {
        Path::new("/workspaces")
            .join(&self.id)
            .join(sanitize_resource_name(worktree_name))
    }

    /// In-container working directory corresponding exactly to the host invocation path.
    #[must_use]
    pub fn container_workdir(&self, worktree_name: &str) -> PathBuf {
        self.container_workspace(worktree_name)
            .join(&self.relative_workdir)
    }

    /// Label set common to every resource owned by this project.
    #[must_use]
    pub fn ownership_labels(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("io.codex-start.managed".into(), "true".into()),
            ("io.codex-start.project".into(), self.id.clone()),
        ])
    }
}

/// Application-owned paths. Construction does not create them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub environments: PathBuf,
    pub projects: PathBuf,
    pub homes: PathBuf,
    pub worktrees: PathBuf,
    pub runtime: PathBuf,
}

impl AppPaths {
    /// Resolve from the current process environment using XDG variables when set.
    pub fn discover() -> Result<Self, ProjectError> {
        let home = directories::BaseDirs::new()
            .map(|directories| directories.home_dir().to_path_buf())
            .ok_or(ProjectError::HomeUnavailable)?;
        Self::from_roots(
            &home,
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        )
    }

    /// Deterministic constructor used by tests and embedding applications.
    pub fn from_roots(
        home: &Path,
        xdg_config_home: Option<PathBuf>,
        xdg_data_home: Option<PathBuf>,
        xdg_cache_home: Option<PathBuf>,
    ) -> Result<Self, ProjectError> {
        if !home.is_absolute() {
            return Err(ProjectError::PathNotAbsolute {
                kind: "home",
                path: home.to_path_buf(),
            });
        }
        let config_root = validate_xdg_root(
            "XDG_CONFIG_HOME",
            xdg_config_home.unwrap_or_else(|| home.join(".config")),
        )?;
        let data_root = validate_xdg_root(
            "XDG_DATA_HOME",
            xdg_data_home.unwrap_or_else(|| home.join(".local/share")),
        )?;
        let cache_root = validate_xdg_root(
            "XDG_CACHE_HOME",
            xdg_cache_home.unwrap_or_else(|| home.join(".cache")),
        )?;
        let config = config_root.join("codex-start");
        let data = data_root.join("codex-start");
        let cache = cache_root.join("codex-start");
        Ok(Self {
            environments: config.join("environments"),
            projects: config.join("projects"),
            homes: data.join("homes"),
            worktrees: data.join("worktrees"),
            runtime: cache.join("runtime"),
            config,
            data,
            cache,
        })
    }

    #[must_use]
    pub fn global_config(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    #[must_use]
    pub fn environment_manifest(&self, name: &str) -> PathBuf {
        self.environments
            .join(format!("{}.toml", sanitize_resource_name(name)))
    }

    #[must_use]
    pub fn managed_home(&self, name: &str) -> PathBuf {
        self.homes.join(sanitize_resource_name(name))
    }
}

/// Stable lowercase name accepted by container runtimes and filesystem layouts.
#[must_use]
pub fn sanitize_resource_name(value: &str) -> String {
    sanitize(value, true)
}

/// Safe Git branch path component (dots are replaced to avoid special ref forms).
#[must_use]
pub fn sanitize_branch_component(value: &str) -> String {
    sanitize(value, false)
}

fn sanitize(value: &str, allow_dot: bool) -> String {
    let mut result = String::with_capacity(value.len());
    let mut pending_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        let allowed = character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || character == '-'
            || allow_dot && character == '.';
        if allowed {
            if pending_separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character);
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    while result.ends_with(['-', '.']) {
        result.pop();
    }
    let result = result.trim_start_matches(['-', '.']);
    if result.is_empty() {
        "unnamed".into()
    } else {
        result
            .chars()
            .take(63)
            .collect::<String>()
            .trim_end_matches(['-', '.'])
            .to_owned()
    }
}

/// Short BLAKE3 of a normalized absolute path.
#[must_use]
pub fn canonical_path_hash(path: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    #[cfg(unix)]
    hasher.update(path.as_os_str().as_bytes());
    #[cfg(not(unix))]
    hasher.update(normalized_path_text(path).as_bytes());
    hasher.finalize().to_hex()[..16].to_owned()
}

#[cfg(not(unix))]
fn normalized_path_text(path: &Path) -> String {
    let mut normalized = String::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                normalized.push_str(&prefix.as_os_str().to_string_lossy().to_ascii_lowercase());
            }
            Component::RootDir => normalized.push('/'),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.ends_with('/') {
                    normalized.push('/');
                }
                normalized.push_str("..");
            }
            Component::Normal(value) => {
                if !normalized.ends_with('/') {
                    normalized.push('/');
                }
                normalized.push_str(&value.to_string_lossy());
            }
        }
    }
    normalized
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, ProjectError> {
    let canonical = path
        .canonicalize()
        .map_err(|source| ProjectError::Canonicalize {
            kind,
            path: path.to_path_buf(),
            source,
        })?;
    if !canonical.is_dir() {
        return Err(ProjectError::NotDirectory {
            kind,
            path: canonical,
        });
    }
    Ok(canonical)
}

fn relative_below(path: &Path, root: &Path) -> Result<PathBuf, ProjectError> {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| ProjectError::OutsideRoot {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        })
}

fn safe_name_from_path(path: &Path) -> String {
    path.file_name().map_or_else(
        || "unnamed".into(),
        |name| sanitize_resource_name(&name.to_string_lossy()),
    )
}

fn validate_xdg_root(kind: &'static str, path: PathBuf) -> Result<PathBuf, ProjectError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ProjectError::PathNotAbsolute { kind, path })
    }
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("could not determine the user's home directory")]
    HomeUnavailable,
    #[error("{kind} path must be absolute: {path}")]
    PathNotAbsolute { kind: &'static str, path: PathBuf },
    #[error("failed to canonicalize {kind} {path}: {source}")]
    Canonicalize {
        kind: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{kind} is not a directory: {path}")]
    NotDirectory { kind: &'static str, path: PathBuf },
    #[error("working directory {path} is outside project root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::tempdir;

    #[test]
    fn linked_worktrees_share_id_and_git_config() {
        let temporary = tempdir().expect("tempdir");
        let project = temporary.path().join("my project");
        let common = project.join(".git");
        let main_src = project.join("src");
        let linked = temporary.path().join("linked");
        let linked_src = linked.join("src");
        for path in [&common, &main_src, &linked_src] {
            std::fs::create_dir_all(path).expect("directory");
        }
        let main = ProjectIdentity::git(&project, &common, &main_src).expect("main");
        let other = ProjectIdentity::git(&linked, &common, &linked_src).expect("linked");
        assert_eq!(main.id, other.id);
        assert_eq!(main.display_name, "my-project");
        assert_eq!(main.relative_workdir, Path::new("src"));
        let app = AppPaths::from_roots(
            Path::new("/home/test"),
            Some(temporary.path().join("config")),
            Some(temporary.path().join("data")),
            Some(temporary.path().join("cache")),
        )
        .expect("paths");
        assert_eq!(
            main.config_path(&app),
            common
                .canonicalize()
                .expect("canonical common")
                .join("codex-start.toml")
        );
        assert_eq!(main.config_path(&app), other.config_path(&app));
    }

    #[test]
    fn non_git_config_is_hashed_under_xdg_config() {
        let temporary = tempdir().expect("tempdir");
        let src = temporary.path().join("src");
        std::fs::create_dir(&src).expect("src");
        let identity = ProjectIdentity::directory(temporary.path(), &src).expect("identity");
        let app = AppPaths::from_roots(
            Path::new("/home/me"),
            Some(PathBuf::from("/config")),
            Some(PathBuf::from("/data")),
            Some(PathBuf::from("/cache")),
        )
        .expect("paths");
        assert_eq!(
            identity.config_path(&app),
            Path::new("/config/codex-start/projects").join(format!("{}.toml", identity.id))
        );
        assert_eq!(
            identity.container_workdir("Feature One"),
            Path::new("/workspaces")
                .join(&identity.id)
                .join("feature-one/src")
        );
    }

    #[test]
    fn rejects_cwd_outside_root() {
        let root = tempdir().expect("root");
        let outside = tempdir().expect("outside");
        assert!(matches!(
            ProjectIdentity::directory(root.path(), outside.path()),
            Err(ProjectError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn xdg_defaults_and_overrides_are_exact() {
        let defaults =
            AppPaths::from_roots(Path::new("/home/a"), None, None, None).expect("defaults");
        assert_eq!(
            defaults.global_config(),
            Path::new("/home/a/.config/codex-start/config.toml")
        );
        assert_eq!(
            defaults.homes,
            Path::new("/home/a/.local/share/codex-start/homes")
        );
        assert_eq!(
            defaults.runtime,
            Path::new("/home/a/.cache/codex-start/runtime")
        );
        assert!(
            AppPaths::from_roots(
                Path::new("/home/a"),
                Some(PathBuf::from("relative")),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn sanitizer_matches_runtime_constraints() {
        assert_eq!(sanitize_resource_name(" Hello / WORLD... "), "hello-world");
        assert_eq!(
            sanitize_branch_component("Feature.One / Two"),
            "feature-one-two"
        );
        assert_eq!(sanitize_resource_name("..."), "unnamed");
    }

    proptest! {
        #[test]
        fn sanitization_is_nonempty_and_uses_safe_ascii(input in ".*") {
            let value = sanitize_resource_name(&input);
            prop_assert!(!value.is_empty());
            prop_assert!(value.len() <= 63);
            prop_assert!(value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')));
            prop_assert!(!value.starts_with(['-', '.']));
            prop_assert!(!value.ends_with(['-', '.']));
        }
    }
}
