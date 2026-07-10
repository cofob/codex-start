//! Secret-reference resolution and file-only container injection.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_SECRET_BYTES: usize = 1_048_576;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    command::{CommandOutput, CommandSpec, run_capture},
    error::{HostError, Result},
    paths::set_private_file,
    runtime::{MountKind, MountRequest},
};

/// A global-only reference to secret material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SecretSource {
    /// Read an exact host environment variable.
    Env {
        /// Variable name.
        name: String,
    },
    /// Read a permission-checked host file.
    File {
        /// File path; leading `~/` is expanded.
        path: PathBuf,
    },
    /// Run a trusted argv vector without a shell and consume stdout.
    Command {
        /// Non-empty executable and argument vector.
        argv: Vec<String>,
    },
    /// Read a native OS keychain entry.
    Keychain {
        /// Service/collection identifier.
        service: String,
        /// Account/username identifier.
        account: String,
    },
}

/// A named secret plus its optional target environment variable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    /// Secret value provider.
    pub source: SecretSource,
    /// Child environment variable populated by the Rust init process.
    #[serde(default)]
    pub target_env: Option<String>,
    /// Fail launch if the source is unavailable.
    #[serde(default = "default_true")]
    pub required: bool,
}

/// Resolved secret files retained for the lifetime of a container run.
#[derive(Debug)]
pub struct SecretBundle {
    directory: TempDir,
}

impl SecretBundle {
    /// Resolve selected named secrets into a private temporary directory.
    pub fn resolve(
        definitions: &BTreeMap<String, SecretSpec>,
        selected: &[String],
        runtime_parent: &Path,
    ) -> Result<Self> {
        fs::create_dir_all(runtime_parent)
            .map_err(|source| HostError::io(runtime_parent, source))?;
        let directory = tempfile::Builder::new()
            .prefix("secrets-")
            .tempdir_in(runtime_parent)
            .map_err(|source| HostError::io(runtime_parent, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .map_err(|source| HostError::io(directory.path(), source))?;
        }

        let mut env_map = BTreeMap::new();
        for name in selected {
            validate_name(name)?;
            let spec = definitions.get(name).ok_or_else(|| {
                HostError::Config(format!("project references undefined secret {name:?}"))
            })?;
            let secret = match resolve_source(&spec.source) {
                Ok(value) => value,
                Err(error) if !spec.required => {
                    tracing::warn!(secret = name, %error, "optional secret is unavailable");
                    continue;
                }
                Err(error) => return Err(error),
            };
            let path = directory.path().join(name);
            let mut file =
                fs::File::create(&path).map_err(|source| HostError::io(&path, source))?;
            file.write_all(secret.expose_secret().as_bytes())
                .map_err(|source| HostError::io(&path, source))?;
            file.sync_all()
                .map_err(|source| HostError::io(&path, source))?;
            set_private_file(&path)?;
            if let Some(target) = &spec.target_env {
                validate_env_name(target)?;
                env_map.insert(target.clone(), format!("/run/secrets/{name}"));
            }
        }
        let map_path = directory.path().join("map.json");
        let serialized = serde_json::to_vec(&env_map)
            .map_err(|error| HostError::Serialization(error.to_string()))?;
        fs::write(&map_path, serialized).map_err(|source| HostError::io(&map_path, source))?;
        set_private_file(&map_path)?;
        Ok(Self { directory })
    }

    /// Read-only mount exposing resolved files to the init process.
    pub fn mount(&self) -> MountRequest {
        MountRequest {
            kind: MountKind::Bind,
            source: Some(self.directory.path().as_os_str().to_owned()),
            target: PathBuf::from("/run/secrets"),
            read_only: true,
        }
    }

    /// Container path of the environment-to-file mapping.
    pub fn container_map_path() -> PathBuf {
        PathBuf::from("/run/secrets/map.json")
    }
}

fn resolve_source(source: &SecretSource) -> Result<SecretString> {
    let value = match source {
        SecretSource::Env { name } => {
            validate_env_name(name)?;
            env::var(name).map(SecretString::from).map_err(|_| {
                HostError::Config(format!("secret environment variable {name} is not set"))
            })?
        }
        SecretSource::File { path } => read_secret_file(&expand_tilde(path)?)?,
        SecretSource::Command { argv } => {
            let (program, args) = argv.split_first().ok_or_else(|| {
                HostError::Config("secret command argv must not be empty".to_owned())
            })?;
            let output = run_secret_command(
                &CommandSpec::new(program).args(args.iter()),
                "secret provider command",
            )?;
            secret_from_output(output, "secret command")?
        }
        SecretSource::Keychain { service, account } => resolve_keychain(service, account)?,
    };
    validate_secret_value(&value)?;
    Ok(value)
}

#[cfg(target_os = "macos")]
fn resolve_keychain(service: &str, account: &str) -> Result<SecretString> {
    let output = run_secret_command(
        &CommandSpec::new("security").args([
            "find-generic-password",
            "-w",
            "-s",
            service,
            "-a",
            account,
        ]),
        "macOS keychain lookup",
    )?;
    secret_from_output(output, "macOS keychain")
}

#[cfg(target_os = "linux")]
fn resolve_keychain(service: &str, account: &str) -> Result<SecretString> {
    let output = run_secret_command(
        &CommandSpec::new("secret-tool").args(["lookup", "service", service, "account", account]),
        "Linux keychain lookup",
    )?;
    secret_from_output(output, "Linux keychain")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resolve_keychain(_service: &str, _account: &str) -> Result<SecretString> {
    Err(HostError::Config(
        "native keychain secrets are supported only on macOS and Linux".to_owned(),
    ))
}

fn read_secret_file(path: &Path) -> Result<SecretString> {
    let before = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(HostError::Config(format!(
            "secret source must be a non-symlink regular file: {}",
            path.display()
        )));
    }
    let file = fs::File::open(path).map_err(|source| HostError::io(path, source))?;
    let descriptor = file
        .metadata()
        .map_err(|source| HostError::io(path, source))?;
    let after = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    if !descriptor.is_file()
        || after.file_type().is_symlink()
        || !same_file_identity(&before, &descriptor)
        || !same_file_identity(&descriptor, &after)
    {
        return Err(HostError::Config(format!(
            "secret source changed identity while opening: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if descriptor.permissions().mode() & 0o077 != 0 {
            return Err(HostError::Config(format!(
                "secret file {} must not be accessible by group or others",
                path.display()
            )));
        }
    }
    if descriptor.len() > MAX_SECRET_BYTES as u64 {
        return Err(HostError::Config(format!(
            "secret file {} exceeds {MAX_SECRET_BYTES} bytes",
            path.display()
        )));
    }
    let mut value = Zeroizing::new(String::new());
    file.take((MAX_SECRET_BYTES + 1) as u64)
        .read_to_string(&mut value)
        .map_err(|source| HostError::io(path, source))?;
    if value.len() > MAX_SECRET_BYTES {
        return Err(HostError::Config(format!(
            "secret file {} exceeds {MAX_SECRET_BYTES} bytes",
            path.display()
        )));
    }
    Ok(SecretString::from(
        value.trim_end_matches(['\r', '\n']).to_owned(),
    ))
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

fn secret_from_output(mut output: CommandOutput, source: &str) -> Result<SecretString> {
    if output.stdout.len() > MAX_SECRET_BYTES {
        output.stdout.zeroize();
        return Err(HostError::Config(format!(
            "{source} returned more than {MAX_SECRET_BYTES} bytes"
        )));
    }
    output.stderr.zeroize();
    let bytes = std::mem::take(&mut output.stdout);
    let value = match String::from_utf8(bytes) {
        Ok(value) => value,
        Err(error) => {
            let utf8_error = error.utf8_error();
            let _bytes = Zeroizing::new(error.into_bytes());
            return Err(HostError::Config(format!(
                "{source} returned non-UTF-8 output: {utf8_error}"
            )));
        }
    };
    let value = Zeroizing::new(value);
    Ok(SecretString::from(
        value.trim_end_matches(['\r', '\n']).to_owned(),
    ))
}

fn validate_secret_value(value: &SecretString) -> Result<()> {
    let value = value.expose_secret();
    if value.len() > MAX_SECRET_BYTES || value.contains('\0') {
        return Err(HostError::Config(
            "secret value is too large or contains a NUL byte".to_owned(),
        ));
    }
    Ok(())
}

fn run_secret_command(spec: &CommandSpec, description: &str) -> Result<CommandOutput> {
    let mut output = run_capture(spec)?;
    if output.status.success() {
        Ok(output)
    } else {
        output.stdout.zeroize();
        output.stderr.zeroize();
        Err(HostError::Config(format!(
            "{description} failed with status {} (stderr suppressed)",
            output.status
        )))
    }
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let Some(value) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if value == "~" || value.starts_with("~/") {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| HostError::Config("HOME is not set".to_owned()))?;
        return Ok(if value == "~" {
            home
        } else {
            home.join(&value[2..])
        });
    }
    Ok(path.to_path_buf())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(HostError::Config(format!("invalid secret name {name:?}")));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(HostError::Config(format!(
            "invalid target environment variable {name:?}"
        )));
    }
    Ok(())
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::{SecretBundle, SecretSource, SecretSpec};

    #[test]
    fn resolves_file_secret_without_placing_value_in_map() {
        let root = tempfile::tempdir().expect("tempdir");
        let source = root.path().join("token");
        fs::write(&source, "sensitive\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("permissions");
        }
        let definitions = BTreeMap::from([(
            "token".to_owned(),
            SecretSpec {
                source: SecretSource::File { path: source },
                target_env: Some("TOKEN".to_owned()),
                required: true,
            },
        )]);
        let bundle = SecretBundle::resolve(&definitions, &["token".to_owned()], root.path())
            .expect("bundle");
        let map = fs::read_to_string(bundle.directory.path().join("map.json")).expect("map");
        assert!(map.contains("TOKEN"));
        assert!(!map.contains("sensitive"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_and_nul_secret_files() {
        use std::os::unix::{fs::PermissionsExt as _, fs::symlink};

        let root = tempfile::tempdir().expect("tempdir");
        let source = root.path().join("source");
        fs::write(&source, "secret").expect("source");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).expect("permissions");
        let link = root.path().join("link");
        symlink(&source, &link).expect("symlink");
        let definition = |path| {
            BTreeMap::from([(
                "token".to_owned(),
                SecretSpec {
                    source: SecretSource::File { path },
                    target_env: Some("TOKEN".to_owned()),
                    required: true,
                },
            )])
        };
        assert!(
            SecretBundle::resolve(&definition(link), &["token".to_owned()], root.path()).is_err()
        );

        let nul = root.path().join("nul");
        fs::write(&nul, b"secret\0tail").expect("nul");
        fs::set_permissions(&nul, fs::Permissions::from_mode(0o600)).expect("permissions");
        assert!(
            SecretBundle::resolve(&definition(nul), &["token".to_owned()], root.path()).is_err()
        );
    }
}
