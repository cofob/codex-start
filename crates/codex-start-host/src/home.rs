//! Selectable, shared Codex home management.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};

use codex_start_core::HomeConfig as CoreHomeConfig;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile};
use walkdir::{DirEntry, WalkDir};

use crate::{
    error::{HostError, Result},
    paths::{AppPaths, create_private_dir, ensure_regular_file_or_missing, set_private_file},
    runtime::{MountKind, MountRequest},
};

const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-journal", "-shm"];

/// Storage mode for a named Codex home.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HomeKind {
    /// codex-start-owned state under XDG data.
    Managed,
    /// The invoking user's native `~/.codex` and `~/.agents`.
    Host,
    /// A user-selected Codex home directory.
    Path,
}

/// Named home declaration from configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeSpec {
    /// Storage behavior.
    pub kind: HomeKind,
    /// Storage directory for a managed home; defaults to the configuration key.
    #[serde(default)]
    pub storage_name: Option<String>,
    /// Codex home for `kind = "path"`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Optional user `.agents` directory override.
    #[serde(default)]
    pub agents_path: Option<PathBuf>,
}

impl Default for HomeSpec {
    fn default() -> Self {
        Self {
            kind: HomeKind::Managed,
            storage_name: None,
            path: None,
            agents_path: None,
        }
    }
}

/// Fully resolved host paths for one Codex identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHome {
    /// Configuration name.
    pub name: String,
    /// Storage behavior.
    pub kind: HomeKind,
    /// Host Codex home bound to `/home/codex/.codex`.
    pub codex_home: PathBuf,
    /// Host agents directory bound to `/home/codex/.agents`.
    pub agents_home: PathBuf,
}

impl ResolvedHome {
    /// Resolve a named home without creating directories. Used by dry-run
    /// planning so inspection has no mutable home-state side effects.
    pub fn preview(name: &str, spec: &HomeSpec, paths: &AppPaths) -> Result<Self> {
        validate_home_name(name)?;
        if let Some(storage_name) = &spec.storage_name {
            validate_home_name(storage_name)?;
        }
        let host_home = env::var_os("HOME").map(PathBuf::from);
        let (codex_home, agents_home) = match spec.kind {
            HomeKind::Managed => {
                let root = paths
                    .homes_dir()
                    .join(spec.storage_name.as_deref().unwrap_or(name));
                (root.join(".codex"), root.join(".agents"))
            }
            HomeKind::Host => {
                let root =
                    host_home.ok_or_else(|| HostError::Config("HOME is not set".to_owned()))?;
                (root.join(".codex"), root.join(".agents"))
            }
            HomeKind::Path => {
                let codex = spec.path.clone().ok_or_else(|| {
                    HostError::Config(format!("home {name:?} has kind=path but no path"))
                })?;
                let agents = spec.agents_path.clone().unwrap_or_else(|| {
                    codex
                        .parent()
                        .map_or_else(|| PathBuf::from(".agents"), |parent| parent.join(".agents"))
                });
                (expand_tilde(&codex)?, expand_tilde(&agents)?)
            }
        };
        ensure_distinct_roots(&codex_home, &agents_home)?;
        Ok(Self {
            name: name.to_owned(),
            kind: spec.kind.clone(),
            codex_home,
            agents_home,
        })
    }

    /// Resolve and initialize a named home.
    pub fn resolve(name: &str, spec: &HomeSpec, paths: &AppPaths) -> Result<Self> {
        let resolved = Self::preview(name, spec, paths)?;
        let codex_home = &resolved.codex_home;
        let agents_home = &resolved.agents_home;
        ensure_private_directory(codex_home)?;
        ensure_private_directory(agents_home)?;
        ensure_private_directory(&agents_home.join("skills"))?;
        ensure_private_directory(&agents_home.join("plugins"))?;
        Ok(resolved)
    }

    /// Bind mounts required by a development container.
    pub fn mounts(&self) -> Vec<MountRequest> {
        vec![
            MountRequest {
                kind: MountKind::Bind,
                source: Some(self.codex_home.as_os_str().to_owned()),
                target: PathBuf::from("/home/codex/.codex"),
                read_only: false,
            },
            MountRequest {
                kind: MountKind::Bind,
                source: Some(self.agents_home.as_os_str().to_owned()),
                target: PathBuf::from("/home/codex/.agents"),
                read_only: false,
            },
        ]
    }

    /// Acquire a shared session lock. Imports and exports require exclusivity.
    pub fn lock_shared(&self) -> Result<HomeLock> {
        HomeLock::acquire(&self.codex_home, false)
    }

    /// Acquire an exclusive maintenance lock.
    pub fn lock_exclusive(&self) -> Result<HomeLock> {
        HomeLock::acquire(&self.codex_home, true)
    }

    /// Copy supported Codex state from another home while excluding live journals and locks.
    pub fn import_from(&self, source: &Path, agents_source: Option<&Path>) -> Result<CopySummary> {
        let _guard = self.lock_exclusive()?;
        let codex_files = copy_tree(source, &self.codex_home)?;
        let agents_files =
            agents_source.map_or(Ok(0), |path| copy_tree(path, &self.agents_home))?;
        Ok(CopySummary {
            codex_files,
            agents_files,
        })
    }

    /// Export supported Codex state to an empty or existing directory.
    pub fn export_to(
        &self,
        destination: &Path,
        agents_destination: Option<&Path>,
    ) -> Result<CopySummary> {
        let _guard = self.lock_exclusive()?;
        let codex_files = copy_tree(&self.codex_home, destination)?;
        let agents_files =
            agents_destination.map_or(Ok(0), |path| copy_tree(&self.agents_home, path))?;
        Ok(CopySummary {
            codex_files,
            agents_files,
        })
    }
}

/// Discover usable homes without mutating configuration or home contents.
pub fn discover_home_configs(paths: &AppPaths) -> Result<BTreeMap<String, CoreHomeConfig>> {
    discover_home_configs_at(paths, env::var_os("HOME").as_deref().map(Path::new))
}

fn discover_home_configs_at(
    paths: &AppPaths,
    host_home: Option<&Path>,
) -> Result<BTreeMap<String, CoreHomeConfig>> {
    let mut homes = BTreeMap::new();
    let homes_dir = paths.homes_dir();
    for entry in fs::read_dir(&homes_dir).map_err(|source| HostError::io(&homes_dir, source))? {
        let entry = entry.map_err(|source| HostError::io(&homes_dir, source))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if entry
            .file_type()
            .map_err(|source| HostError::io(entry.path(), source))?
            .is_dir()
            && valid_home_name(&name)
        {
            homes.insert(name.clone(), CoreHomeConfig::Managed { name: Some(name) });
        }
    }

    if let Some(host_home) = host_home.filter(|path| path.is_absolute())
        && usable_host_home(host_home)?
    {
        homes
            .entry("host".to_owned())
            .or_insert(CoreHomeConfig::Host);
    }
    Ok(homes)
}

fn usable_host_home(home: &Path) -> Result<bool> {
    let mut present = false;
    for path in [home.join(".codex"), home.join(".agents")] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                present = true;
            }
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(HostError::io(path, source)),
        }
    }
    Ok(present)
}

/// Number of files copied for the two directories that make up a Codex home.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CopySummary {
    /// Files copied from or into `.codex`.
    pub codex_files: usize,
    /// Files copied from or into `.agents`.
    pub agents_files: usize,
}

impl CopySummary {
    /// Total number of copied files and symbolic links.
    #[must_use]
    pub const fn total(self) -> usize {
        self.codex_files + self.agents_files
    }
}

/// Held advisory home lock.
#[derive(Debug)]
pub struct HomeLock {
    file: File,
}

impl HomeLock {
    fn acquire(home: &Path, exclusive: bool) -> Result<Self> {
        ensure_private_directory(home)?;
        let path = home.join(".codex-start.lock");
        let existed = ensure_regular_file_or_missing(&path)?;
        let before = existed
            .then(|| fs::symlink_metadata(&path).map_err(|source| HostError::io(&path, source)))
            .transpose()?;
        let mut options = File::options();
        options.create(true).truncate(false).read(true).write(true);
        let file = options
            .open(&path)
            .map_err(|source| HostError::io(&path, source))?;
        verify_open_file(&path, &file, before.as_ref())?;
        set_private_file(&path)?;
        if exclusive {
            file.try_lock_exclusive().map_err(|source| {
                HostError::Runtime(format!(
                    "Codex home {} is active and cannot be modified: {source}",
                    home.display()
                ))
            })?;
        } else {
            FileExt::try_lock_shared(&file).map_err(|source| {
                HostError::Runtime(format!(
                    "Codex home {} is being modified: {source}",
                    home.display()
                ))
            })?;
        }
        Ok(Self { file })
    }
}

impl Drop for HomeLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn validate_home_name(name: &str) -> Result<()> {
    if !valid_home_name(name) {
        return Err(HostError::Config(format!(
            "invalid Codex home name {name:?}"
        )));
    }
    Ok(())
}

fn valid_home_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let Some(text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if text == "~" || text.starts_with("~/") {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| HostError::Config("HOME is not set".to_owned()))?;
        return Ok(if text == "~" {
            home
        } else {
            home.join(&text[2..])
        });
    }
    Ok(path.to_path_buf())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<usize> {
    let source = prepare_source_root(source)?;
    let destination_absolute = absolute_path(destination)?;
    ensure_roots_do_not_overlap(&source, &destination_absolute)?;
    let destination = prepare_destination_root(&destination_absolute)?;
    ensure_roots_do_not_overlap(&source, &destination)?;

    let live_before = scan_live_databases(&source)?;
    let staging = Builder::new()
        .prefix(".codex-start-copy-")
        .tempdir_in(&destination)
        .map_err(|source| HostError::io(&destination, source))?;
    ensure_private_directory(staging.path())?;
    copy_tree_entries(&source, staging.path(), &live_before)?;

    let mut live_databases = live_before;
    live_databases.extend(scan_live_databases(&source)?);
    remove_live_database_copies(staging.path(), &live_databases)?;
    copy_tree_entries(staging.path(), &destination, &BTreeSet::new())
}

fn copy_tree_entries(
    source: &Path,
    destination: &Path,
    live_databases: &BTreeSet<PathBuf>,
) -> Result<usize> {
    let mut copied = 0;
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| traversal_error(source, error))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| unsafe_path(entry.path(), "copy escaped its source directory"))?;
        if relative.as_os_str().is_empty() || should_skip(relative, live_databases) {
            continue;
        }
        copied += copy_entry(&entry, relative, destination)?;
    }
    Ok(copied)
}

fn copy_entry(entry: &DirEntry, relative: &Path, destination: &Path) -> Result<usize> {
    let target = destination.join(relative);
    if entry.file_type().is_dir() {
        ensure_private_directory(&target)?;
        Ok(0)
    } else if entry.file_type().is_file() {
        copy_regular_file(entry.path(), &target)?;
        Ok(1)
    } else if entry.file_type().is_symlink() {
        copy_symbolic_link(entry.path(), relative, &target)?;
        Ok(1)
    } else {
        Err(unsafe_path(
            entry.path(),
            "only directories, regular files, and symbolic links can be copied",
        ))
    }
}

fn copy_regular_file(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| unsafe_path(target, "copy target has no parent directory"))?;
    ensure_private_directory(parent)?;
    ensure_regular_file_or_missing(target)?;

    let before = fs::symlink_metadata(source).map_err(|error| HostError::io(source, error))?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(unsafe_path(source, "copy source is not a regular file"));
    }
    let mut options = File::options();
    options.read(true);
    let mut input = options
        .open(source)
        .map_err(|source_error| HostError::io(source, source_error))?;
    let metadata = verify_open_file(source, &input, Some(&before))?;

    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|error| HostError::io(parent, error))?;
    io::copy(&mut input, &mut temporary)
        .map_err(|source_error| HostError::io(source, source_error))?;
    apply_source_permissions(temporary.path(), &metadata)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source_error| HostError::io(temporary.path(), source_error))?;
    ensure_regular_file_or_missing(target)?;
    temporary
        .persist(target)
        .map_err(|error| HostError::io(target, error.error))?;
    Ok(())
}

fn apply_source_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let owner_permissions = metadata.permissions().mode() & 0o700;
        fs::set_permissions(path, fs::Permissions::from_mode(owner_permissions))
            .map_err(|source| HostError::io(path, source))?;
    }
    #[cfg(not(unix))]
    fs::set_permissions(path, metadata.permissions())
        .map_err(|source| HostError::io(path, source))?;
    Ok(())
}

fn verify_open_file(
    path: &Path,
    file: &File,
    before: Option<&fs::Metadata>,
) -> Result<fs::Metadata> {
    let descriptor = file
        .metadata()
        .map_err(|source| HostError::io(path, source))?;
    let after = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if !descriptor.is_file()
        || !after.is_file()
        || after.file_type().is_symlink()
        || !same_file_identity(&descriptor, &after)
        || before.is_some_and(|metadata| !same_file_identity(metadata, &descriptor))
    {
        return Err(unsafe_path(
            path,
            "regular file changed identity while it was being opened",
        ));
    }
    Ok(descriptor)
}

#[cfg(unix)]
fn same_file_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    first.dev() == second.dev() && first.ino() == second.ino()
}

#[cfg(not(unix))]
fn same_file_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    first.len() == second.len()
        && first.modified().ok() == second.modified().ok()
        && first.created().ok() == second.created().ok()
}

fn copy_symbolic_link(source: &Path, relative: &Path, target: &Path) -> Result<()> {
    let link = fs::read_link(source).map_err(|error| HostError::io(source, error))?;
    validate_symbolic_link(relative, &link, source)?;
    let parent = target
        .parent()
        .ok_or_else(|| unsafe_path(target, "symbolic-link target has no parent directory"))?;
    ensure_private_directory(parent)?;
    ensure_missing_path(target)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&link, target).map_err(|error| HostError::io(target, error))?;
    #[cfg(windows)]
    {
        let target_is_dir = fs::metadata(source)
            .map_err(|error| HostError::io(source, error))?
            .is_dir();
        if target_is_dir {
            std::os::windows::fs::symlink_dir(&link, target)
        } else {
            std::os::windows::fs::symlink_file(&link, target)
        }
        .map_err(|error| HostError::io(target, error))?;
    }
    Ok(())
}

fn validate_symbolic_link(relative: &Path, link: &Path, source: &Path) -> Result<()> {
    let mut depth = relative.parent().map_or(0, |parent| {
        parent
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
    });
    for component in link.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(
                    source,
                    "symbolic link escapes the copied directory",
                ));
            }
        }
    }
    Ok(())
}

fn scan_live_databases(source: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut databases = BTreeSet::new();
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| traversal_error(source, error))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| unsafe_path(entry.path(), "database scan escaped its source directory"))?;
        if let Some(database) = database_for_sidecar(relative) {
            databases.insert(database);
        }
    }
    Ok(databases)
}

fn database_for_sidecar(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    SQLITE_SIDECAR_SUFFIXES.iter().find_map(|suffix| {
        name.strip_suffix(suffix)
            .filter(|database| !database.is_empty())
            .map(|database| path.with_file_name(database))
    })
}

fn remove_live_database_copies(root: &Path, databases: &BTreeSet<PathBuf>) -> Result<()> {
    for database in databases {
        let path = root.join(database);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                fs::remove_file(&path).map_err(|source| HostError::io(&path, source))?;
            }
            Ok(_) => {
                return Err(unsafe_path(
                    &path,
                    "live database path unexpectedly resolved to a directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(HostError::io(&path, source)),
        }
    }
    Ok(())
}

fn should_skip(relative: &Path, live_databases: &BTreeSet<PathBuf>) -> bool {
    let name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    name == ".codex-start.lock"
        || SQLITE_SIDECAR_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
        || live_databases.contains(relative)
        || relative
            .components()
            .any(|component| component.as_os_str() == ".tmp")
}

fn prepare_source_root(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_path(path)?;
    ensure_no_symbolic_link_components(&absolute)?;
    let metadata = fs::symlink_metadata(&absolute).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            HostError::NotFound(format!("Codex home {}", absolute.display()))
        } else {
            HostError::io(&absolute, source)
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_path(
            &absolute,
            "copy source must be a directory that is not a symbolic link",
        ));
    }
    absolute
        .canonicalize()
        .map_err(|source| HostError::io(&absolute, source))
}

fn prepare_destination_root(path: &Path) -> Result<PathBuf> {
    ensure_private_directory(path)?;
    path.canonicalize()
        .map_err(|source| HostError::io(path, source))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    let absolute = absolute_path(path)?;
    ensure_no_symbolic_link_components(&absolute)?;
    create_private_dir(&absolute)?;
    ensure_no_symbolic_link_components(&absolute)
}

fn ensure_no_symbolic_link_components(path: &Path) -> Result<()> {
    let trusted_prefix = trusted_path_prefix(path);
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(unsafe_path(path, "parent traversal is not allowed"));
        }
        current.push(component.as_os_str());
        if trusted_prefix
            .as_ref()
            .is_some_and(|trusted| trusted.starts_with(&current))
        {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(unsafe_path(
                    &current,
                    "an existing path component is a symbolic link",
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(unsafe_path(
                    &current,
                    "an existing parent component is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(source) => return Err(HostError::io(&current, source)),
        }
    }
    Ok(())
}

fn trusted_path_prefix(path: &Path) -> Option<PathBuf> {
    [
        env::var_os("HOME").map(PathBuf::from),
        Some(env::temp_dir()),
    ]
    .into_iter()
    .flatten()
    .filter(|candidate| path.starts_with(candidate))
    .max_by_key(|candidate| candidate.components().count())
}

fn ensure_missing_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(unsafe_path(
            path,
            "refusing to replace an existing path with a symbolic link",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(HostError::io(path, source)),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|source| HostError::io(".", source))?
            .join(path)
    };
    if absolute
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(unsafe_path(&absolute, "parent traversal is not allowed"));
    }
    Ok(absolute)
}

fn ensure_distinct_roots(first: &Path, second: &Path) -> Result<()> {
    let first = absolute_path(first)?;
    let second = absolute_path(second)?;
    ensure_roots_do_not_overlap(&first, &second)
}

fn ensure_roots_do_not_overlap(first: &Path, second: &Path) -> Result<()> {
    if first == second || first.starts_with(second) || second.starts_with(first) {
        return Err(unsafe_path(
            second,
            format!("copy roots overlap with {}", first.display()),
        ));
    }
    Ok(())
}

fn traversal_error(root: &Path, error: walkdir::Error) -> HostError {
    HostError::Io {
        path: error.path().unwrap_or(root).to_path_buf(),
        source: error
            .into_io_error()
            .unwrap_or_else(|| io::Error::other("directory traversal failed")),
    }
}

fn unsafe_path(path: impl Into<PathBuf>, reason: impl Into<String>) -> HostError {
    HostError::UnsafePath {
        path: path.into(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use codex_start_core::HomeConfig as CoreHomeConfig;

    use super::{HomeKind, HomeSpec, ResolvedHome, discover_home_configs_at};
    use crate::{error::HostError, paths::AppPaths};

    fn test_paths(root: &std::path::Path) -> AppPaths {
        AppPaths {
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
        }
    }

    fn managed_home(root: &std::path::Path) -> ResolvedHome {
        let paths = test_paths(root);
        paths.ensure().expect("paths");
        ResolvedHome::resolve("default", &HomeSpec::default(), &paths).expect("home")
    }

    #[test]
    fn managed_storage_name_selects_storage_directory() {
        let root = tempfile::tempdir().expect("root");
        let paths = test_paths(root.path());
        paths.ensure().expect("paths");
        let spec = HomeSpec {
            storage_name: Some("shared-storage".to_owned()),
            ..HomeSpec::default()
        };
        let home = ResolvedHome::resolve("work", &spec, &paths).expect("home");
        assert_eq!(
            home.codex_home,
            paths.homes_dir().join("shared-storage/.codex")
        );
        assert_eq!(home.kind, HomeKind::Managed);
    }

    #[test]
    fn discovers_only_usable_managed_and_host_homes() {
        let root = tempfile::tempdir().expect("root");
        let paths = test_paths(root.path());
        paths.ensure().expect("paths");
        fs::create_dir(paths.homes_dir().join("work")).expect("managed");
        fs::create_dir(paths.homes_dir().join(".invalid")).expect("invalid");
        let host = root.path().join("host");
        fs::create_dir_all(host.join(".codex")).expect("host Codex home");

        let homes = discover_home_configs_at(&paths, Some(&host)).expect("discover");
        assert_eq!(
            homes.get("work"),
            Some(&CoreHomeConfig::Managed {
                name: Some("work".to_owned())
            })
        );
        assert_eq!(homes.get("host"), Some(&CoreHomeConfig::Host));
        assert!(!homes.contains_key(".invalid"));
    }

    #[cfg(unix)]
    #[test]
    fn does_not_advertise_a_host_home_with_symlinked_state() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let paths = test_paths(root.path());
        paths.ensure().expect("paths");
        let host = root.path().join("host");
        fs::create_dir(&host).expect("host");
        fs::create_dir(root.path().join("actual-codex")).expect("actual");
        symlink(root.path().join("actual-codex"), host.join(".codex")).expect("symlink");

        let homes = discover_home_configs_at(&paths, Some(&host)).expect("discover");
        assert!(!homes.contains_key("host"));
    }

    #[test]
    fn home_mounts_expose_complete_codex_and_agents_state() {
        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let mounts = home.mounts();
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(home.codex_home.as_os_str())
        );
        assert_eq!(mounts[0].target, std::path::Path::new("/home/codex/.codex"));
        assert!(!mounts[0].read_only);
        assert_eq!(
            mounts[1].source.as_deref(),
            Some(home.agents_home.as_os_str())
        );
        assert_eq!(
            mounts[1].target,
            std::path::Path::new("/home/codex/.agents")
        );
        assert!(!mounts[1].read_only);
        assert!(home.agents_home.join("skills").is_dir());
        assert!(home.agents_home.join("plugins").is_dir());
    }

    #[test]
    fn import_copies_codex_and_agents_but_excludes_live_databases() {
        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source-codex");
        let agents = root.path().join("source-agents");
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(agents.join("skills/example")).expect("agents");
        fs::write(source.join("config.toml"), "model='test'").expect("config");
        fs::write(source.join("state.sqlite"), "live database").expect("database");
        fs::write(source.join("state.sqlite-wal"), "live journal").expect("wal");
        fs::write(source.join("cache.sqlite"), "quiescent database").expect("cache database");
        fs::write(source.join("Cargo.lock"), "legitimate plugin lock").expect("lock file");
        fs::write(agents.join("skills/example/SKILL.md"), "# Example").expect("skill");

        let summary = home.import_from(&source, Some(&agents)).expect("import");
        assert_eq!(summary.codex_files, 3);
        assert_eq!(summary.agents_files, 1);
        assert_eq!(summary.total(), 4);
        assert!(home.codex_home.join("config.toml").is_file());
        assert!(home.codex_home.join("cache.sqlite").is_file());
        assert!(!home.codex_home.join("state.sqlite").exists());
        assert!(!home.codex_home.join("state.sqlite-wal").exists());
        assert!(home.codex_home.join("Cargo.lock").is_file());
        assert!(home.agents_home.join("skills/example/SKILL.md").is_file());
    }

    #[test]
    fn explicit_missing_agents_source_is_an_error() {
        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source");
        fs::create_dir(&source).expect("source");
        let error = home
            .import_from(&source, Some(&root.path().join("missing-agents")))
            .expect_err("missing agents source");
        assert!(matches!(error, HostError::NotFound(_)));
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_existing_symbolic_link_file_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("config.toml"), "replacement").expect("source file");
        let outside = root.path().join("outside.toml");
        fs::write(&outside, "unchanged").expect("outside");
        symlink(&outside, home.codex_home.join("config.toml")).expect("target symlink");

        assert!(home.import_from(&source, None).is_err());
        assert_eq!(
            fs::read_to_string(outside).expect("outside read"),
            "unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_existing_symbolic_link_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source");
        fs::create_dir_all(source.join("nested")).expect("source");
        fs::write(source.join("nested/config.toml"), "replacement").expect("source file");
        let outside = root.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        symlink(&outside, home.codex_home.join("nested")).expect("parent symlink");

        assert!(home.import_from(&source, None).is_err());
        assert!(!outside.join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_symbolic_link_destination_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        fs::write(home.codex_home.join("config.toml"), "source").expect("source file");
        let outside = root.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        let destination = root.path().join("destination");
        symlink(&outside, &destination).expect("destination symlink");

        assert!(home.export_to(&destination, None).is_err());
        assert!(!outside.join("config.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_symbolic_link_that_escapes_source() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source");
        fs::create_dir(&source).expect("source");
        symlink("../outside", source.join("escape")).expect("source symlink");

        assert!(home.import_from(&source, None).is_err());
        assert!(!home.codex_home.join("escape").exists());
    }
}
