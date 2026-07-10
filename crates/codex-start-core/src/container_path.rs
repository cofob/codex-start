//! Validated paths interpreted inside Linux containers.

use std::{
    fmt,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// An absolute, normalized POSIX path whose meaning is independent of the host OS.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContainerPath(String);

impl ContainerPath {
    /// Validate a path using container (POSIX) semantics rather than host semantics.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, ContainerPathError> {
        let path = path.as_ref();
        let value = path.to_str().ok_or(ContainerPathError::NotUnicode)?;
        Self::parse(value)
    }

    /// Validate a UTF-8 container path.
    pub fn parse(value: &str) -> Result<Self, ContainerPathError> {
        if !value.starts_with('/') {
            return Err(ContainerPathError::NotAbsolute);
        }
        if value.contains('\0') {
            return Err(ContainerPathError::Nul);
        }
        if value.contains('\\') {
            return Err(ContainerPathError::Backslash);
        }
        if value != "/" && (value.ends_with('/') || value.contains("//")) {
            return Err(ContainerPathError::NotNormalized);
        }
        if value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        {
            return Err(ContainerPathError::Traversal);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// Component-aware prefix test using POSIX separators.
    #[must_use]
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0 == prefix
            || self
                .0
                .strip_prefix(prefix)
                .is_some_and(|suffix| prefix == "/" || suffix.starts_with('/'))
    }

    /// Append one already-separated POSIX component.
    pub fn join_component(&self, component: &str) -> Result<Self, ContainerPathError> {
        if component.is_empty()
            || component.contains(['/', '\\', '\0'])
            || matches!(component, "." | "..")
        {
            return Err(ContainerPathError::InvalidComponent);
        }
        let value = if self.is_root() {
            format!("/{component}")
        } else {
            format!("{}/{component}", self.0)
        };
        Self::parse(&value)
    }

    /// Append a relative host path while rendering its components with `/`.
    pub fn join_relative(&self, relative: &Path) -> Result<Self, ContainerPathError> {
        let mut joined = self.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(component) => {
                    let component = component.to_str().ok_or(ContainerPathError::NotUnicode)?;
                    joined = joined.join_component(component)?;
                }
                Component::ParentDir => return Err(ContainerPathError::Traversal),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ContainerPathError::InvalidComponent);
                }
            }
        }
        Ok(joined)
    }

    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        PathBuf::from(self.0)
    }
}

impl fmt::Display for ContainerPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ContainerPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContainerPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ContainerPathError {
    #[error("container path is not Unicode")]
    NotUnicode,
    #[error("container path must start with `/`")]
    NotAbsolute,
    #[error("container path contains a NUL byte")]
    Nul,
    #[error("container path contains a host-style backslash")]
    Backslash,
    #[error("container path contains `.` or `..` traversal")]
    Traversal,
    #[error("container path contains repeated or trailing separators")]
    NotNormalized,
    #[error("container path component is empty, absolute, or contains a separator")]
    InvalidComponent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_paths_without_host_path_semantics() {
        let path = ContainerPath::parse("/home/codex/project").unwrap();
        assert!(path.starts_with("/home/codex"));
        assert!(!path.starts_with("/home/code"));
        for invalid in [
            "relative",
            r"C:\\workspace",
            r"/home\\codex",
            "/home/../root",
            "/home//codex",
            "/home/codex/",
        ] {
            assert!(ContainerPath::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn preserves_the_existing_string_wire_shape() {
        let path = ContainerPath::parse("/run/secrets/token").unwrap();
        assert_eq!(serde_json::to_value(&path).unwrap(), "/run/secrets/token");
        assert_eq!(
            serde_json::from_value::<ContainerPath>(serde_json::json!("/run/secrets/token"))
                .unwrap(),
            path
        );
    }

    #[test]
    fn joins_host_relative_components_with_posix_separators() {
        let base = ContainerPath::parse("/workspaces/project").unwrap();
        let joined = base.join_relative(Path::new("src/components")).unwrap();
        assert_eq!(joined.as_str(), "/workspaces/project/src/components");
    }
}
