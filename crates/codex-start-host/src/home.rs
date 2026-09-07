//! Selectable, shared Codex home management.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use codex_start_core::HomeConfig as CoreHomeConfig;
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile};
use walkdir::{DirEntry, WalkDir};

use crate::{
    error::{HostError, Result},
    paths::{AppPaths, create_private_dir, ensure_regular_file_or_missing, set_private_file},
    runtime::{MountKind, MountRequest},
};

const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-journal", "-shm"];
const CHAT_DIRECTORIES: [&str; 2] = ["sessions", "archived_sessions"];
const CHAT_ARTIFACT_DIRECTORIES: [&str; 4] =
    ["attachments", "generated_images", "plans", "visualizations"];
const THREAD_HISTORY_TABLES: [&str; 4] = [
    "thread_history_projection_state",
    "thread_turns",
    "thread_items",
    "thread_realtime_items",
];
const ROLLOUT_INDEX_READ_LIMIT: u64 = 4 * 1024 * 1024;

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

    /// Add missing chat history and projects from another Codex home.
    pub fn backfill_from(&self, source: &Path) -> Result<BackfillSummary> {
        let _guard = self.lock_exclusive()?;
        let source = prepare_source_root(source)?;
        let destination = prepare_destination_root(&absolute_path(&self.codex_home)?)?;
        ensure_roots_do_not_overlap(&source, &destination)?;

        let mut summary = BackfillSummary::default();
        let mut target_chats = indexed_chat_files(&destination)?;
        for directory in CHAT_DIRECTORIES {
            backfill_directory(
                &source,
                &destination,
                directory,
                true,
                &mut target_chats,
                &mut summary,
            )?;
        }
        for directory in CHAT_ARTIFACT_DIRECTORIES {
            backfill_directory(
                &source,
                &destination,
                directory,
                false,
                &mut target_chats,
                &mut summary,
            )?;
        }
        let source_chats = indexed_chat_files(&source)?;
        let thread_records = read_target_threads(
            &target_chats,
            &source_chats,
            &destination,
            self.kind == HomeKind::Managed,
        )?;
        backfill_thread_history(&source, &destination, &thread_records, &mut summary)?;
        backfill_thread_index(&destination, &thread_records, &mut summary)?;
        backfill_projects(&source, &destination, &mut summary)?;
        if self.kind == HomeKind::Managed {
            summary.rollout_paths_rewritten =
                normalize_managed_rollout_paths(&destination, &self.codex_home)?;
        }
        Ok(summary)
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

/// Files and project records found during a history backfill.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BackfillSummary {
    /// Source chat rollout files.
    pub chats_found: usize,
    /// Chat rollout files added to the target home.
    pub chats_copied: usize,
    /// Related attachment and generated artifact files added to the target home.
    pub files_copied: usize,
    /// Source project records.
    pub projects_found: usize,
    /// Project records added to the target home.
    pub projects_copied: usize,
    /// Chat records added to the target Codex thread index.
    pub threads_indexed: usize,
    /// Paginated turn-history records added to the target home.
    pub history_records_copied: usize,
    /// Host paths changed to the path used inside a codex-start container.
    pub rollout_paths_rewritten: usize,
}

impl CopySummary {
    /// Total number of copied files and symbolic links.
    #[must_use]
    pub const fn total(self) -> usize {
        self.codex_files + self.agents_files
    }
}

fn indexed_chat_files(root: &Path) -> Result<BTreeMap<std::ffi::OsString, PathBuf>> {
    let mut files = BTreeMap::new();
    for directory in CHAT_DIRECTORIES {
        let path = root.join(directory);
        if !path.exists() {
            continue;
        }
        for entry in WalkDir::new(&path).min_depth(1).follow_links(false) {
            let entry = entry.map_err(|error| traversal_error(&path, error))?;
            if entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            {
                files
                    .entry(entry.file_name().to_os_string())
                    .or_insert_with(|| entry.path().to_path_buf());
            }
        }
    }
    Ok(files)
}

fn backfill_directory(
    source_root: &Path,
    destination_root: &Path,
    directory: &str,
    chats: bool,
    target_chats: &mut BTreeMap<std::ffi::OsString, PathBuf>,
    summary: &mut BackfillSummary,
) -> Result<()> {
    let source = source_root.join(directory);
    match fs::symlink_metadata(&source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HostError::io(&source, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(unsafe_path(
                &source,
                "history source must be a directory that is not a symbolic link",
            ));
        }
        Ok(_) => {}
    }

    for entry in WalkDir::new(&source).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|error| traversal_error(&source, error))?;
        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(unsafe_path(
                entry.path(),
                "history contains a file type that cannot be copied",
            ));
        }
        if chats
            && entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(source_root)
            .map_err(|_| unsafe_path(entry.path(), "history copy escaped its source directory"))?;
        let mut target = destination_root.join(relative);
        if chats {
            summary.chats_found += 1;
            if let Some(existing) = target_chats.get(entry.file_name()) {
                target.clone_from(existing);
            }
        }
        if copy_new_regular_file(entry.path(), &target)? {
            if chats {
                summary.chats_copied += 1;
                target_chats.insert(entry.file_name().to_os_string(), target);
            } else {
                summary.files_copied += 1;
            }
        }
    }
    Ok(())
}

fn copy_new_regular_file(source: &Path, target: &Path) -> Result<bool> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            return Ok(false);
        }
        Ok(_) => {
            return Err(unsafe_path(
                target,
                "history target exists and is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(HostError::io(target, source)),
    }

    let parent = target
        .parent()
        .ok_or_else(|| unsafe_path(target, "history target has no parent directory"))?;
    ensure_private_directory(parent)?;
    for attempt in 0..3 {
        let before = fs::symlink_metadata(source).map_err(|error| HostError::io(source, error))?;
        if !before.is_file() || before.file_type().is_symlink() {
            return Err(unsafe_path(source, "history source is not a regular file"));
        }
        let mut input = File::open(source).map_err(|error| HostError::io(source, error))?;
        verify_open_file(source, &input, Some(&before))?;
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|error| HostError::io(parent, error))?;
        let copied =
            io::copy(&mut input, &mut temporary).map_err(|error| HostError::io(source, error))?;
        let after = fs::symlink_metadata(source).map_err(|error| HostError::io(source, error))?;
        if !same_file_identity(&before, &after)
            || copied != before.len()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            if attempt < 2 {
                std::thread::yield_now();
                continue;
            }
            return Err(HostError::Runtime(format!(
                "chat history {} kept changing during backfill; run the command again",
                source.display()
            )));
        }
        apply_source_permissions(temporary.path(), &before)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| HostError::io(temporary.path(), error))?;
        return match temporary.persist_noclobber(target) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                ensure_regular_file_or_missing(target)?;
                Ok(false)
            }
            Err(error) => Err(HostError::io(target, error.error)),
        };
    }
    unreachable!("copy attempts return or continue")
}

#[derive(Debug)]
struct ThreadRecord {
    id: String,
    rollout_path: String,
    created_at: i64,
    updated_at: i64,
    source: String,
    model_provider: String,
    cwd: String,
    preview: String,
    sandbox_policy: String,
    approval_mode: String,
    archived: bool,
    git_sha: Option<String>,
    git_branch: Option<String>,
    git_origin_url: Option<String>,
    cli_version: String,
    history_mode: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    thread_source: Option<String>,
}

#[derive(Default)]
struct RolloutDetails {
    response_preview: Option<String>,
    event_preview: Option<String>,
    completed_preview: Option<String>,
    sandbox_policy: Option<String>,
    approval_mode: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

fn read_target_threads(
    target_chats: &BTreeMap<std::ffi::OsString, PathBuf>,
    source_chats: &BTreeMap<std::ffi::OsString, PathBuf>,
    target_root: &Path,
    managed: bool,
) -> Result<Vec<ThreadRecord>> {
    let mut records = Vec::new();
    for (name, target_path) in target_chats {
        let metadata_path = source_chats.get(name).unwrap_or(target_path);
        if let Some(record) = read_rollout_thread(metadata_path, target_path, target_root, managed)?
        {
            records.push(record);
        }
    }
    Ok(records)
}

fn read_rollout_thread(
    source_path: &Path,
    target_path: &Path,
    target_root: &Path,
    managed: bool,
) -> Result<Option<ThreadRecord>> {
    let mut bytes = Vec::new();
    File::open(target_path)
        .map_err(|error| HostError::io(target_path, error))?
        .take(ROLLOUT_INDEX_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|error| HostError::io(target_path, error))?;
    let mut values = bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok());
    let Some(metadata) = values.find(|value| value["type"] == "session_meta") else {
        return Ok(None);
    };
    let payload = &metadata["payload"];
    let Some(id) = payload["id"].as_str() else {
        return Ok(None);
    };
    if uuid::Uuid::parse_str(id).is_err()
        || !target_path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.ends_with(&format!("-{id}.jsonl")))
    {
        return Ok(None);
    }

    let modified_ms = fs::metadata(source_path)
        .map_err(|error| HostError::io(source_path, error))?
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default();
    let created_at_ms = uuid_v7_timestamp_ms(id).unwrap_or(modified_ms);
    let updated_at_ms = modified_ms.max(created_at_ms);
    let relative = target_path.strip_prefix(target_root).map_err(|_| {
        unsafe_path(
            target_path,
            "backfilled rollout escaped the target Codex home",
        )
    })?;
    let rollout_path = if managed {
        Path::new("/home/codex/.codex").join(relative)
    } else {
        target_path.to_path_buf()
    }
    .to_str()
    .ok_or_else(|| HostError::Config("rollout path must be valid UTF-8".to_owned()))?
    .to_owned();

    let details = scan_rollout_details(values);
    let preview = details
        .completed_preview
        .or(details.event_preview)
        .or(details.response_preview)
        .unwrap_or_default();
    let git = &payload["git"];
    Ok(Some(ThreadRecord {
        id: id.to_owned(),
        rollout_path,
        created_at: created_at_ms / 1000,
        updated_at: updated_at_ms / 1000,
        source: json_database_value(&payload["source"]).unwrap_or_else(|| "cli".to_owned()),
        model_provider: payload["model_provider"]
            .as_str()
            .unwrap_or("openai")
            .to_owned(),
        cwd: payload["cwd"].as_str().unwrap_or("/").to_owned(),
        preview,
        sandbox_policy: details
            .sandbox_policy
            .unwrap_or_else(|| "{\"type\":\"danger-full-access\"}".to_owned()),
        approval_mode: details
            .approval_mode
            .unwrap_or_else(|| "on-request".to_owned()),
        archived: relative
            .components()
            .next()
            .is_some_and(|part| part.as_os_str() == "archived_sessions"),
        git_sha: git["commit_hash"].as_str().map(str::to_owned),
        git_branch: git["branch"].as_str().map(str::to_owned),
        git_origin_url: git["repository_url"].as_str().map(str::to_owned),
        cli_version: payload["cli_version"].as_str().unwrap_or("").to_owned(),
        history_mode: payload["history_mode"]
            .as_str()
            .unwrap_or("legacy")
            .to_owned(),
        model: details.model.filter(|value| !value.is_empty()),
        reasoning_effort: details.reasoning_effort.filter(|value| !value.is_empty()),
        thread_source: json_database_value(&payload["thread_source"]),
    }))
}

fn scan_rollout_details(values: impl Iterator<Item = serde_json::Value>) -> RolloutDetails {
    let mut details = RolloutDetails::default();
    for value in values {
        let record_payload = &value["payload"];
        match value["type"].as_str() {
            Some("turn_context") => {
                details.sandbox_policy.get_or_insert_with(|| {
                    json_database_value(&record_payload["sandbox_policy"])
                        .unwrap_or_else(|| "{\"type\":\"danger-full-access\"}".to_owned())
                });
                details.approval_mode.get_or_insert_with(|| {
                    record_payload["approval_policy"]
                        .as_str()
                        .unwrap_or("on-request")
                        .to_owned()
                });
                details
                    .model
                    .get_or_insert_with(|| record_payload["model"].as_str().unwrap_or("").into());
                details
                    .reasoning_effort
                    .get_or_insert_with(|| record_payload["effort"].as_str().unwrap_or("").into());
            }
            Some("response_item")
                if record_payload["type"] == "message"
                    && record_payload["role"] == "user"
                    && details.response_preview.is_none() =>
            {
                details.response_preview = content_text(&record_payload["content"]);
            }
            Some("event_msg") if record_payload["type"] == "user_message" => {
                if details.event_preview.is_none() {
                    details.event_preview = record_payload["message"].as_str().map(str::to_owned);
                }
            }
            Some("event_msg")
                if record_payload["type"] == "item_completed"
                    && record_payload["item"]["type"]
                        .as_str()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("userMessage")) =>
            {
                if details.completed_preview.is_none() {
                    details.completed_preview = content_text(&record_payload["item"]["content"]);
                }
            }
            _ => {}
        }
    }
    details
}

fn content_text(content: &serde_json::Value) -> Option<String> {
    let text = content
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn json_database_value(value: &serde_json::Value) -> Option<String> {
    if value.is_null() {
        None
    } else if let Some(value) = value.as_str() {
        Some(value.to_owned())
    } else {
        Some(value.to_string())
    }
}

fn uuid_v7_timestamp_ms(id: &str) -> Option<i64> {
    let compact = id.replace('-', "");
    if compact.len() != 32 || compact.as_bytes().get(12) != Some(&b'7') {
        return None;
    }
    i64::from_str_radix(compact.get(..12)?, 16).ok()
}

fn backfill_thread_history(
    source_root: &Path,
    destination_root: &Path,
    threads: &[ThreadRecord],
    summary: &mut BackfillSummary,
) -> Result<()> {
    if threads.is_empty() {
        return Ok(());
    }
    let Some(source_path) = latest_versioned_database(source_root, "thread_history_")? else {
        return Ok(());
    };
    let Some(destination_path) = latest_versioned_database(destination_root, "thread_history_")?
    else {
        return Ok(());
    };
    let mut destination = open_state_database(&destination_path, false)?;
    destination
        .execute(
            "ATTACH DATABASE ?1 AS backfill_source",
            [source_path.to_string_lossy().as_ref()],
        )
        .map_err(|error| state_database_error(&destination_path, &error))?;
    destination
        .execute_batch(
            "CREATE TEMP TABLE backfill_thread_ids (
                thread_id TEXT PRIMARY KEY
            ) WITHOUT ROWID;",
        )
        .map_err(|error| state_database_error(&destination_path, &error))?;
    let transaction = destination
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| state_database_error(&destination_path, &error))?;
    for thread in threads {
        transaction
            .execute(
                "INSERT OR IGNORE INTO backfill_thread_ids VALUES (?1)",
                [&thread.id],
            )
            .map_err(|error| state_database_error(&destination_path, &error))?;
    }
    for table in THREAD_HISTORY_TABLES {
        if !has_table_in_schema(&transaction, "main", table)?
            || !has_table_in_schema(&transaction, "backfill_source", table)?
        {
            continue;
        }
        let target_columns = table_columns(&transaction, "main", table)?;
        let source_columns = table_columns(&transaction, "backfill_source", table)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let columns = target_columns
            .into_iter()
            .filter(|column| source_columns.contains(column))
            .collect::<Vec<_>>();
        if !columns.iter().any(|column| column == "thread_id") || columns.is_empty() {
            continue;
        }
        let columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", ");
        let table = quote_identifier(table);
        let statement = format!(
            "INSERT OR IGNORE INTO main.{table} ({columns})
             SELECT {columns} FROM backfill_source.{table}
             WHERE thread_id IN (SELECT thread_id FROM temp.backfill_thread_ids)"
        );
        summary.history_records_copied += transaction
            .execute(&statement, [])
            .map_err(|error| state_database_error(&destination_path, &error))?;
    }
    transaction
        .commit()
        .map_err(|error| state_database_error(&destination_path, &error))?;
    Ok(())
}

fn backfill_thread_index(
    destination_root: &Path,
    threads: &[ThreadRecord],
    summary: &mut BackfillSummary,
) -> Result<()> {
    let Some(destination_path) = latest_state_database(destination_root)? else {
        return Ok(());
    };
    let mut destination = open_state_database(&destination_path, false)?;
    if !has_table(&destination, "threads")? {
        return Ok(());
    }
    let columns = table_columns(&destination, "main", "threads")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let required = [
        "id",
        "rollout_path",
        "created_at",
        "updated_at",
        "source",
        "model_provider",
        "cwd",
        "title",
        "sandbox_policy",
        "approval_mode",
    ];
    if required.iter().any(|column| !columns.contains(*column)) {
        return Ok(());
    }
    let transaction = destination
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| state_database_error(&destination_path, &error))?;
    for thread in threads {
        let mut fields = vec![
            ("id", thread.id.clone().into()),
            ("rollout_path", thread.rollout_path.clone().into()),
            ("created_at", thread.created_at.into()),
            ("updated_at", thread.updated_at.into()),
            ("source", thread.source.clone().into()),
            ("model_provider", thread.model_provider.clone().into()),
            ("cwd", thread.cwd.clone().into()),
            ("title", thread.preview.clone().into()),
            ("sandbox_policy", thread.sandbox_policy.clone().into()),
            ("approval_mode", thread.approval_mode.clone().into()),
            (
                "has_user_event",
                i64::from(!thread.preview.is_empty()).into(),
            ),
            ("archived", i64::from(thread.archived).into()),
            ("git_sha", option_sql_value(thread.git_sha.clone())),
            ("git_branch", option_sql_value(thread.git_branch.clone())),
            (
                "git_origin_url",
                option_sql_value(thread.git_origin_url.clone()),
            ),
            ("cli_version", thread.cli_version.clone().into()),
            ("first_user_message", thread.preview.clone().into()),
            ("model", option_sql_value(thread.model.clone())),
            (
                "reasoning_effort",
                option_sql_value(thread.reasoning_effort.clone()),
            ),
            ("created_at_ms", (thread.created_at * 1000).into()),
            ("updated_at_ms", (thread.updated_at * 1000).into()),
            (
                "thread_source",
                option_sql_value(thread.thread_source.clone()),
            ),
            ("preview", thread.preview.clone().into()),
            ("recency_at", thread.updated_at.into()),
            ("recency_at_ms", (thread.updated_at * 1000).into()),
            ("history_mode", thread.history_mode.clone().into()),
        ];
        fields.retain(|(name, _)| columns.contains(*name));
        let names = fields
            .iter()
            .map(|(name, _)| quote_identifier(name))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = (1..=fields.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let values = fields.into_iter().map(|(_, value)| value);
        let statement = format!("INSERT OR IGNORE INTO threads ({names}) VALUES ({placeholders})");
        summary.threads_indexed += transaction
            .execute(&statement, params_from_iter(values))
            .map_err(|error| state_database_error(&destination_path, &error))?;
    }
    transaction
        .commit()
        .map_err(|error| state_database_error(&destination_path, &error))?;
    Ok(())
}

fn option_sql_value(value: Option<String>) -> rusqlite::types::Value {
    value.map_or(rusqlite::types::Value::Null, Into::into)
}

fn has_table_in_schema(connection: &Connection, schema: &str, name: &str) -> Result<bool> {
    let statement = format!(
        "SELECT 1 FROM {}.sqlite_schema WHERE type = 'table' AND name = ?1",
        quote_identifier(schema)
    );
    connection
        .query_row(&statement, [name], |_| Ok(()))
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| HostError::Runtime(format!("could not inspect Codex state: {error}")))
}

fn table_columns(connection: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    let statement = format!(
        "SELECT name FROM {}.pragma_table_info(?1) ORDER BY cid",
        quote_identifier(schema)
    );
    connection
        .prepare(&statement)
        .and_then(|mut statement| {
            statement
                .query_map([table], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .map_err(|error| HostError::Runtime(format!("could not inspect Codex state: {error}")))
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[derive(Debug)]
struct ProjectRecord {
    id: String,
    name: String,
    metadata: String,
    position: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    roots: Vec<String>,
}

fn backfill_projects(
    source_root: &Path,
    destination_root: &Path,
    summary: &mut BackfillSummary,
) -> Result<()> {
    let Some(source_path) = latest_state_database(source_root)? else {
        return Ok(());
    };
    let source = open_state_database(&source_path, true)?;
    if !has_table(&source, "projects")? || !has_table(&source, "project_roots")? {
        return Ok(());
    }
    let source_projects = read_projects(&source)?;
    summary.projects_found = source_projects.len();
    let Some(destination_path) = latest_state_database(destination_root)? else {
        return Ok(());
    };
    let mut destination = open_state_database(&destination_path, false)?;
    if !has_table(&destination, "projects")? || !has_table(&destination, "project_roots")? {
        return Ok(());
    }

    let destination_projects = read_projects(&destination)?;
    let mut target_by_roots = destination_projects
        .iter()
        .filter(|project| !project.roots.is_empty())
        .map(|project| (project.roots.clone(), project.id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut target_ids = destination_projects
        .iter()
        .map(|project| project.id.clone())
        .collect::<BTreeSet<_>>();

    let transaction = destination
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| state_database_error(&destination_path, &error))?;
    for project in source_projects {
        if !project.roots.is_empty() && target_by_roots.contains_key(&project.roots) {
            continue;
        }
        let id = if target_ids.contains(&project.id) {
            uuid::Uuid::new_v4().to_string()
        } else {
            project.id
        };
        transaction
            .execute(
                "INSERT INTO projects (id, name, metadata, position, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    project.name,
                    project.metadata,
                    project.position,
                    project.created_at_ms,
                    project.updated_at_ms,
                ],
            )
            .map_err(|error| state_database_error(&destination_path, &error))?;
        for (position, path) in project.roots.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO project_roots (project_id, position, path) VALUES (?1, ?2, ?3)",
                    params![id, i64::try_from(position).unwrap_or(i64::MAX), path],
                )
                .map_err(|error| state_database_error(&destination_path, &error))?;
        }
        if !project.roots.is_empty() {
            target_by_roots.insert(project.roots, id.clone());
        }
        target_ids.insert(id);
        summary.projects_copied += 1;
    }
    transaction
        .commit()
        .map_err(|error| state_database_error(&destination_path, &error))?;
    Ok(())
}

fn normalize_managed_rollout_paths(root: &Path, configured_root: &Path) -> Result<usize> {
    let Some(database_path) = latest_state_database(root)? else {
        return Ok(0);
    };
    let database = open_state_database(&database_path, false)?;
    if !has_table(&database, "threads")? {
        return Ok(0);
    }
    let mut prefixes = BTreeSet::new();
    for candidate in [root, configured_root] {
        let candidate = candidate.to_str().ok_or_else(|| {
            HostError::Config("managed Codex home path must be valid UTF-8".to_owned())
        })?;
        prefixes.insert(format!(
            "{}{separator}",
            candidate.trim_end_matches(['/', '\\']),
            separator = std::path::MAIN_SEPARATOR
        ));
    }
    let mut changed = 0;
    for host_prefix in prefixes {
        changed += database
            .execute(
                "UPDATE threads
                 SET rollout_path = ?2 || substr(rollout_path, length(?1) + 1)
                 WHERE substr(rollout_path, 1, length(?1)) = ?1",
                params![host_prefix, "/home/codex/.codex/"],
            )
            .map_err(|error| state_database_error(&database_path, &error))?;
    }
    Ok(changed)
}

fn latest_state_database(root: &Path) -> Result<Option<PathBuf>> {
    latest_versioned_database(root, "state_")
}

fn latest_versioned_database(root: &Path, prefix: &str) -> Result<Option<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| HostError::io(root, error))? {
        let entry = entry.map_err(|error| HostError::io(root, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(version) = name
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(".sqlite"))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| HostError::io(entry.path(), error))?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            candidates.push((version, entry.path()));
        }
    }
    candidates.sort_by_key(|(version, _)| *version);
    Ok(candidates.pop().map(|(_, path)| path))
}

fn open_state_database(path: &Path, read_only: bool) -> Result<Connection> {
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)
        .map_err(|error| state_database_error(path, &error))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| state_database_error(path, &error))?;
    Ok(connection)
}

fn has_table(connection: &Connection, name: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| HostError::Runtime(format!("could not inspect Codex state: {error}")))
}

fn read_projects(connection: &Connection) -> Result<Vec<ProjectRecord>> {
    let mut statement = connection
        .prepare(
            "SELECT id, name, metadata, position, created_at_ms, updated_at_ms FROM projects ORDER BY position, id",
        )
        .map_err(|error| HostError::Runtime(format!("could not read Codex projects: {error}")))?;
    let records = statement
        .query_map([], |row| {
            Ok(ProjectRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                metadata: row.get(2)?,
                position: row.get(3)?,
                created_at_ms: row.get(4)?,
                updated_at_ms: row.get(5)?,
                roots: Vec::new(),
            })
        })
        .map_err(|error| HostError::Runtime(format!("could not read Codex projects: {error}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| HostError::Runtime(format!("could not read Codex projects: {error}")))?;
    drop(statement);

    records
        .into_iter()
        .map(|mut project| {
            let mut roots = connection
                .prepare("SELECT path FROM project_roots WHERE project_id = ?1 ORDER BY position")
                .map_err(|error| {
                    HostError::Runtime(format!("could not read Codex project roots: {error}"))
                })?;
            project.roots = roots
                .query_map([&project.id], |row| row.get(0))
                .map_err(|error| {
                    HostError::Runtime(format!("could not read Codex project roots: {error}"))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    HostError::Runtime(format!("could not read Codex project roots: {error}"))
                })?;
            Ok(project)
        })
        .collect()
}

fn state_database_error(path: &Path, error: &rusqlite::Error) -> HostError {
    HostError::Runtime(format!("Codex state database {}: {error}", path.display()))
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
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
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
    use rusqlite::{Connection, params};

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
    fn backfill_adds_missing_chats_artifacts_and_projects() {
        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source-codex");
        let source_sessions = source.join("sessions/2026/01/02");
        let target_archive = home.codex_home.join("archived_sessions");
        fs::create_dir_all(&source_sessions).expect("source sessions");
        fs::create_dir_all(&target_archive).expect("target archive");
        let existing = "rollout-2026-01-02T00-00-00-existing.jsonl";
        let missing = "rollout-2026-01-02T00-00-01-missing.jsonl";
        fs::write(source_sessions.join(existing), "source existing\n").expect("source existing");
        fs::write(source_sessions.join(missing), "source missing\n").expect("source missing");
        fs::write(target_archive.join(existing), "target existing\n").expect("target existing");
        fs::create_dir_all(source.join("attachments/thread")).expect("source attachments");
        fs::write(source.join("attachments/thread/input.txt"), "attachment")
            .expect("source attachment");
        fs::write(source.join("auth.json"), "do not copy").expect("source auth");

        create_project_database(
            &source.join("state_5.sqlite"),
            &[
                ("source-shared", "Shared", "/work/shared"),
                ("source-new", "New", "/work/new"),
            ],
        );
        let target_database = home.codex_home.join("state_5.sqlite");
        create_project_database(
            &target_database,
            &[("target-shared", "Shared", "/work/shared")],
        );
        let database = Connection::open(&target_database).expect("target database");
        database
            .execute(
                "INSERT INTO threads VALUES ('host-path', ?1)",
                [home
                    .codex_home
                    .join("sessions/host-path.jsonl")
                    .to_string_lossy()
                    .as_ref()],
            )
            .expect("host rollout path");
        drop(database);

        let summary = home.backfill_from(&source).expect("backfill");
        assert_eq!(summary.chats_found, 2);
        assert_eq!(summary.chats_copied, 1);
        assert_eq!(summary.files_copied, 1);
        assert_eq!(summary.projects_found, 2);
        assert_eq!(summary.projects_copied, 1);
        assert_eq!(summary.rollout_paths_rewritten, 1);
        assert_eq!(
            fs::read_to_string(target_archive.join(existing)).expect("existing chat"),
            "target existing\n"
        );
        assert!(
            home.codex_home
                .join("sessions/2026/01/02")
                .join(missing)
                .is_file()
        );
        assert!(
            home.codex_home
                .join("attachments/thread/input.txt")
                .is_file()
        );
        assert!(!home.codex_home.join("auth.json").exists());
        let database = Connection::open(&target_database).expect("database");
        let projects: i64 = database
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .expect("project count");
        assert_eq!(projects, 2);
        let rollout_path: String = database
            .query_row(
                "SELECT rollout_path FROM threads WHERE id = 'host-path'",
                [],
                |row| row.get(0),
            )
            .expect("rollout path");
        assert_eq!(rollout_path, "/home/codex/.codex/sessions/host-path.jsonl");
        drop(database);

        let repeated = home.backfill_from(&source).expect("repeat backfill");
        assert_eq!(repeated.chats_found, 2);
        assert_eq!(repeated.chats_copied, 0);
        assert_eq!(repeated.files_copied, 0);
        assert_eq!(repeated.projects_found, 2);
        assert_eq!(repeated.projects_copied, 0);
        assert_eq!(repeated.threads_indexed, 0);
        assert_eq!(repeated.history_records_copied, 0);
        assert_eq!(repeated.rollout_paths_rewritten, 0);
    }

    #[test]
    fn backfill_indexes_paginated_threads_and_turn_history() {
        let root = tempfile::tempdir().expect("root");
        let home = managed_home(root.path());
        let source = root.path().join("source-codex");
        let thread_id = "019f4a38-9e22-7d23-9c42-be0b7c13716b";
        let turn_id = "019f4a39-5efa-71b2-a8ac-7678d256f8a8";
        let rollout_name = format!("rollout-2026-07-10T00-00-00-{thread_id}.jsonl");
        let source_sessions = source.join("sessions/2026/07/10");
        fs::create_dir_all(&source_sessions).expect("source sessions");
        let rollout = [
            serde_json::json!({
                "type":"session_meta",
                "payload":{
                    "id":thread_id,
                    "cli_version":"0.153.4",
                    "cwd":"/work/project",
                    "git":{"commit_hash":"abc","branch":"main","repository_url":"ssh://example"},
                    "history_mode":"paginated",
                    "model_provider":"openai",
                    "source":"vscode",
                    "thread_source":"user"
                }
            }),
            serde_json::json!({
                "type":"turn_context",
                "payload":{
                    "approval_policy":"on-request",
                    "effort":"medium",
                    "model":"gpt-test",
                    "sandbox_policy":{"type":"workspace-write"}
                }
            }),
            serde_json::json!({
                "type":"event_msg",
                "payload":{
                    "type":"item_completed",
                    "item":{"type":"UserMessage","content":[{"type":"text","text":"Imported question"}]}
                }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(source_sessions.join(&rollout_name), rollout).expect("rollout");
        create_thread_history_database(&source.join("thread_history_1.sqlite"), thread_id, turn_id);
        create_thread_history_database(&home.codex_home.join("thread_history_1.sqlite"), "", "");
        let state = home.codex_home.join("state_5.sqlite");
        create_thread_index_database(&state);

        let summary = home.backfill_from(&source).expect("backfill");
        assert_eq!(summary.chats_copied, 1);
        assert_eq!(summary.threads_indexed, 1);
        assert_eq!(summary.history_records_copied, 3);
        let database = Connection::open(&state).expect("state");
        let indexed: (String, String, String) = database
            .query_row(
                "SELECT rollout_path, history_mode, preview FROM threads WHERE id = ?1",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("indexed thread");
        assert!(indexed.0.starts_with("/home/codex/.codex/sessions/"));
        assert_eq!(indexed.1, "paginated");
        assert_eq!(indexed.2, "Imported question");
        let history =
            Connection::open(home.codex_home.join("thread_history_1.sqlite")).expect("history");
        let turns: i64 = history
            .query_row(
                "SELECT COUNT(*) FROM thread_turns WHERE thread_id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .expect("turns");
        assert_eq!(turns, 1);
        drop(history);
        drop(database);

        let repeated = home.backfill_from(&source).expect("repeat backfill");
        assert_eq!(repeated.threads_indexed, 0);
        assert_eq!(repeated.history_records_copied, 0);
    }

    fn create_thread_index_database(path: &std::path::Path) {
        Connection::open(path)
            .expect("create state")
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    model_provider TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    title TEXT NOT NULL,
                    sandbox_policy TEXT NOT NULL,
                    approval_mode TEXT NOT NULL,
                    has_user_event INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    git_sha TEXT,
                    git_branch TEXT,
                    git_origin_url TEXT,
                    cli_version TEXT NOT NULL DEFAULT '',
                    first_user_message TEXT NOT NULL DEFAULT '',
                    model TEXT,
                    reasoning_effort TEXT,
                    created_at_ms INTEGER,
                    updated_at_ms INTEGER,
                    thread_source TEXT,
                    preview TEXT NOT NULL DEFAULT '',
                    recency_at INTEGER NOT NULL DEFAULT 0,
                    recency_at_ms INTEGER NOT NULL DEFAULT 0,
                    history_mode TEXT NOT NULL DEFAULT 'legacy'
                );",
            )
            .expect("state schema");
    }

    fn create_thread_history_database(path: &std::path::Path, thread_id: &str, turn_id: &str) {
        let database = Connection::open(path).expect("create history");
        database
            .execute_batch(
                "CREATE TABLE thread_history_projection_state (
                    thread_id TEXT PRIMARY KEY,
                    next_rollout_byte_offset INTEGER NOT NULL,
                    next_rollout_ordinal INTEGER NOT NULL
                );
                CREATE TABLE thread_turns (
                    thread_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    PRIMARY KEY (thread_id, turn_id)
                );
                CREATE TABLE thread_items (
                    thread_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    item_json TEXT NOT NULL,
                    PRIMARY KEY (thread_id, turn_id, item_id)
                );
                CREATE TABLE thread_realtime_items (
                    thread_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    item_type TEXT NOT NULL,
                    item_json TEXT NOT NULL,
                    PRIMARY KEY (thread_id, item_id)
                );",
            )
            .expect("history schema");
        if thread_id.is_empty() {
            return;
        }
        database
            .execute(
                "INSERT INTO thread_history_projection_state VALUES (?1, 100, 3)",
                [thread_id],
            )
            .expect("projection");
        database
            .execute(
                "INSERT INTO thread_turns VALUES (?1, ?2, 1, 'completed')",
                params![thread_id, turn_id],
            )
            .expect("turn");
        database
            .execute(
                "INSERT INTO thread_items VALUES (?1, ?2, 'item-1', 2, 1, '{}')",
                params![thread_id, turn_id],
            )
            .expect("item");
    }

    fn create_project_database(path: &std::path::Path, projects: &[(&str, &str, &str)]) {
        let mut database = Connection::open(path).expect("create database");
        database
            .execute_batch(
                "CREATE TABLE projects (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    metadata TEXT NOT NULL DEFAULT '{}',
                    position INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE project_roots (
                    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL,
                    path TEXT NOT NULL,
                    PRIMARY KEY (project_id, position)
                );
                CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL
                );",
            )
            .expect("schema");
        let transaction = database.transaction().expect("transaction");
        for (position, (id, name, root)) in projects.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO projects VALUES (?1, ?2, '{}', ?3, 1, 1)",
                    params![id, name, i64::try_from(position).expect("position")],
                )
                .expect("project");
            transaction
                .execute(
                    "INSERT INTO project_roots VALUES (?1, 0, ?2)",
                    params![id, root],
                )
                .expect("root");
        }
        transaction.commit().expect("commit");
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
