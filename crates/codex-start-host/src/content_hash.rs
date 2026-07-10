//! Deterministic filesystem hashing for content-addressed container images.

use std::{
    fs,
    io::ErrorKind,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Component, Path},
};

use crate::error::{HostError, Result};

/// Hash every non-ignored entry below `tree_root` relative to `content_root`.
pub(crate) fn hash_tree(
    content_root: &Path,
    tree_root: &Path,
    hasher: &mut blake3::Hasher,
    ignored: impl Fn(&Path) -> bool,
) -> Result<()> {
    ensure_within_content_root(content_root, tree_root)?;
    let mut entries = walkdir::WalkDir::new(tree_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.path().strip_prefix(content_root).is_ok_and(&ignored)
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| HostError::Config(error.to_string()))?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    for entry in entries {
        hash_entry(content_root, entry.path(), hasher)?;
    }
    Ok(())
}

/// Hash one selected filesystem entry relative to `content_root`.
pub(crate) fn hash_entry(
    content_root: &Path,
    path: &Path,
    hasher: &mut blake3::Hasher,
) -> Result<()> {
    ensure_within_content_root(content_root, path)?;
    let relative = path
        .strip_prefix(content_root)
        .map_err(|_| unsafe_path(path, "entry is outside the content root"))?;
    let metadata = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    hash_field(hasher, b"path", relative.as_os_str().as_bytes());
    hash_field(
        hasher,
        b"mode",
        &(metadata.permissions().mode() & 0o7777).to_le_bytes(),
    );
    let file_type = metadata.file_type();
    if file_type.is_file() {
        hash_field(hasher, b"type", b"file");
        let contents = fs::read(path).map_err(|source| HostError::io(path, source))?;
        hash_field(hasher, b"contents", &contents);
    } else if file_type.is_dir() {
        hash_field(hasher, b"type", b"directory");
    } else if file_type.is_symlink() {
        hash_field(hasher, b"type", b"symlink");
        let target = fs::read_link(path).map_err(|source| HostError::io(path, source))?;
        validate_symlink(content_root, path, &target)?;
        hash_field(hasher, b"target", target.as_os_str().as_bytes());
    } else {
        return Err(unsafe_path(
            path,
            "image content contains an unsupported filesystem entry type",
        ));
    }
    Ok(())
}

fn ensure_within_content_root(content_root: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(content_root) {
        return Err(unsafe_path(path, "entry is outside the content root"));
    }
    let canonical_root =
        fs::canonicalize(content_root).map_err(|source| HostError::io(content_root, source))?;
    match fs::canonicalize(path) {
        Ok(canonical_path) if canonical_path.starts_with(&canonical_root) => Ok(()),
        Ok(_) => Err(unsafe_path(path, "entry resolves outside the content root")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(HostError::io(path, source)),
    }
}

fn validate_symlink(content_root: &Path, path: &Path, target: &Path) -> Result<()> {
    if target.is_absolute() || lexically_escapes(content_root, path, target) {
        return Err(unsafe_path(
            path,
            "symbolic-link target escapes the content root",
        ));
    }
    ensure_within_content_root(content_root, path)
}

fn lexically_escapes(content_root: &Path, path: &Path, target: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    let Ok(relative_parent) = parent.strip_prefix(content_root) else {
        return true;
    };
    let mut depth = relative_parent
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

fn hash_field(hasher: &mut blake3::Hasher, label: &[u8], value: &[u8]) {
    hasher.update(&(label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn unsafe_path(path: &Path, reason: impl Into<String>) -> HostError {
    HostError::UnsafePath {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}
