//! Host project catalog and bounded directory browsing before a session starts.

use super::{error, state::State};
use crate::error::Result;
use codex_start_remote::{ProjectId, ProjectInfo};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub fn add(state: &State, path: &str) -> Result<ProjectInfo> {
    let path = std::fs::canonicalize(path).map_err(error)?;
    if !path.is_dir() {
        return Err(error("choose a project directory"));
    }
    let path = path.to_string_lossy().into_owned();
    state.db(|store| {
        let mut projects: Vec<ProjectInfo> =
            serde_json::from_str(&store.value("projects")?.unwrap_or_else(|| "[]".into()))?;
        if let Some(project) = projects.iter().find(|project| project.path == path) {
            return Ok(project.clone());
        }
        if projects.len() >= 1024 {
            return Err(codex_start_remote::Error::Invalid(
                "project limit reached".into(),
            ));
        }
        let project = ProjectInfo {
            id: ProjectId::generate(),
            name: PathBuf::from(&path)
                .file_name()
                .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned()),
            path,
        };
        projects.push(project.clone());
        store.set_value("projects", &serde_json::to_string(&projects)?)?;
        Ok(project)
    })
}

/// One private work directory per client operation. Retrying never replaces its files.
pub async fn create_work_remote(state: &Arc<State>, work_id: &str) -> Result<Value> {
    let state = state.clone();
    let id = work_id.to_owned();
    tokio::task::spawn_blocking(move || create_work(&state, &id).map(|project| json!(project)))
        .await
        .map_err(error)?
}

pub fn create_work(state: &State, work_id: &str) -> Result<ProjectInfo> {
    let id = uuid::Uuid::parse_str(work_id).map_err(|_| error("workId must be a UUID"))?;
    let _lock = state
        .work_projects
        .lock()
        .map_err(|_| error("work project lock poisoned"))?;
    std::fs::create_dir_all(&state.work_directory).map_err(error)?;
    if std::fs::symlink_metadata(&state.work_directory)
        .map_err(error)?
        .file_type()
        .is_symlink()
    {
        return Err(error(
            "the Codex work directory must not be a symbolic link",
        ));
    }
    let root = std::fs::canonicalize(&state.work_directory).map_err(error)?;
    let path = root.join(format!("work-{id}"));
    let projects: Vec<ProjectInfo> = state.db(|store| {
        Ok(serde_json::from_str(
            &store.value("projects")?.unwrap_or_else(|| "[]".into()),
        )?)
    })?;
    let existing = projects
        .iter()
        .find(|project| std::path::Path::new(&project.path) == path);
    if existing.is_none() {
        if projects.len() >= 1024 {
            return Err(error("project limit reached"));
        }
        // create_dir rejects existing files, directories, and symbolic links.
        std::fs::create_dir(&path).map_err(error)?;
    } else {
        let metadata = std::fs::symlink_metadata(&path).map_err(error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(error("the work project is no longer a directory"));
        }
    }
    let project = add(
        state,
        path.to_str()
            .ok_or_else(|| error("work path is not UTF-8"))?,
    )?;
    // Register first: if Git is unavailable, retry can finish setup in the same directory.
    if !path.join(".git").exists() {
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&path)
            .output()
            .map_err(error)?;
        if !output.status.success() {
            return Err(error(format!(
                "cannot initialize the work project: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    Ok(project)
}

pub async fn list(state: &Arc<State>) -> Result<Value> {
    for session in super::sessions::list(state).await {
        if session.kind != "job" {
            let _ = add(state, &session.cwd);
        }
    }
    let mut projects: Vec<ProjectInfo> = state.db(|store| {
        Ok(serde_json::from_str(
            &store.value("projects")?.unwrap_or_else(|| "[]".into()),
        )?)
    })?;
    projects.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.path.cmp(&b.path))
    });
    Ok(json!({"data":projects}))
}

pub fn get(state: &State, id: &str) -> Result<ProjectInfo> {
    let projects: Vec<ProjectInfo> = state.db(|store| {
        Ok(serde_json::from_str(
            &store.value("projects")?.unwrap_or_else(|| "[]".into()),
        )?)
    })?;
    projects
        .into_iter()
        .find(|project| project.id.0 == id)
        .ok_or_else(|| error("project is unavailable"))
}

pub async fn open(state: &Arc<State>, id: &str, profile: Option<&str>) -> Result<Value> {
    let project = get(state, id)?;
    let config = state.config.clone();
    let path = project.path.clone();
    let profile = profile.map(str::to_owned);
    let profile = tokio::task::spawn_blocking(move || {
        let context = crate::configuration::ConfigContext::discover_at(
            config.as_deref(),
            std::path::Path::new(&path),
        )?;
        Ok::<_, crate::error::HostError>(
            context
                .resolve(Some(codex_start_core::ConfigPatch {
                    profile,
                    ..Default::default()
                }))?
                .config
                .selected_profile,
        )
    })
    .await
    .map_err(error)??;
    // An interrupted phone request can retry while its first session is still starting.
    // Serialize only the same project/profile; unrelated work can start in parallel.
    let key = serde_json::to_string(&(&project.id, &profile)).map_err(error)?;
    let lock = {
        let mut opens = state.project_opens.lock().await;
        opens.retain(|_, lock| lock.strong_count() > 0);
        let lock = opens
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        opens.insert(key, Arc::downgrade(&lock));
        lock
    };
    let _opening = lock.lock().await;
    if let Some(session) = super::sessions::list(state)
        .await
        .into_iter()
        .find(|session| {
            session.cwd == project.path
                && session.profile == profile
                && session
                    .capabilities
                    .iter()
                    .any(|capability| capability == "codexRpc")
        })
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(backend) = super::sessions::get(state, &session.id.0).await {
                return Ok(json!({"project":project,"session":backend.info}));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(error(
                    "the project session is still starting; try opening it again",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    let session = super::sessions::create(
        state,
        &json!({"cwd":project.path,"name":session_name(&project.name, profile.as_deref()),"profile":profile,"worktree":false}),
    )
    .await?;
    Ok(json!({"project":project,"session":session}))
}

fn session_name(project: &str, profile: Option<&str>) -> String {
    profile.map_or_else(
        || project.to_owned(),
        |profile| {
            let digest = blake3::hash(profile.as_bytes()).to_hex();
            // A profile has at most 64 ASCII bytes; 32 project characters keep the alias below 255 bytes.
            format!("{project:.32}-{profile}-{}", &digest[..16])
        },
    )
}

pub async fn directories(params: &Value) -> Result<Value> {
    let home = PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| error("host home directory is unavailable"))?,
    );
    let path = params["path"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.clone(), PathBuf::from);
    let path = tokio::fs::canonicalize(path).await.map_err(error)?;
    let after = params["cursor"].as_str().unwrap_or_default();
    let hidden = params["hidden"].as_bool().unwrap_or(false);
    let limit = params["limit"].as_u64().unwrap_or(100).clamp(1, 200) as usize;
    let mut directory = tokio::fs::read_dir(&path).await.map_err(error)?;
    let mut entries = BTreeMap::new();
    while let Some(entry) = directory.next_entry().await.map_err(error)? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.as_str() <= after || (!hidden && name.starts_with('.')) {
            continue;
        }
        let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
            continue;
        };
        entries.insert(
            name.clone(),
            json!({"name":name,"path":entry.path(),"directory":metadata.is_dir()}),
        );
        if entries.len() > limit + 1 {
            entries.pop_last();
        }
    }
    let more = entries.len() > limit;
    if more {
        entries.pop_last();
    }
    let cursor = more
        .then(|| entries.last_key_value().map(|(name, _)| name.clone()))
        .flatten();
    Ok(
        json!({"path":path,"home":home,"parent":path.parent(),"data":entries.into_values().collect::<Vec<_>>(),"nextCursor":cursor}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_state(root: &std::path::Path) -> Arc<State> {
        State::open(
            root.to_owned(),
            None,
            super::super::DaemonOptions::default(),
            "test".into(),
        )
        .unwrap()
    }

    #[test]
    fn work_projects_are_separate_git_repositories_and_retries_keep_files() {
        let root = tempfile::tempdir().unwrap();
        let state = work_state(root.path());
        let id = uuid::Uuid::new_v4().to_string();
        let first = create_work(&state, &id).unwrap();
        let expected = std::fs::canonicalize(&state.work_directory)
            .unwrap()
            .join(format!("work-{id}"));
        assert_eq!(std::path::Path::new(&first.path), expected);
        assert!(expected.join(".git").is_dir());
        std::fs::write(expected.join("notes.txt"), "Keep my work").unwrap();
        let retry = create_work(&state, &id).unwrap();
        assert_eq!(first.id, retry.id);
        assert_eq!(
            std::fs::read_to_string(expected.join("notes.txt")).unwrap(),
            "Keep my work"
        );
        let second = create_work(&state, &uuid::Uuid::new_v4().to_string()).unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.path, second.path);
    }

    #[test]
    fn concurrent_work_retries_register_only_one_project() {
        let root = tempfile::tempdir().unwrap();
        let state = work_state(root.path());
        let id = uuid::Uuid::new_v4().to_string();
        let results = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| create_work(&state, &id).unwrap().id))
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(results.iter().all(|result| result == &results[0]));
        assert_eq!(std::fs::read_dir(&state.work_directory).unwrap().count(), 1);
    }

    #[test]
    fn work_creation_rejects_paths_and_does_not_adopt_existing_directories() {
        let root = tempfile::tempdir().unwrap();
        let state = work_state(root.path());
        for invalid in ["", "../../outside", "/tmp/work", "not-a-uuid"] {
            assert!(create_work(&state, invalid).is_err());
        }
        assert!(!state.work_directory.exists());
        let id = uuid::Uuid::new_v4().to_string();
        let collision = state.work_directory.join(format!("work-{id}"));
        std::fs::create_dir_all(&collision).unwrap();
        std::fs::write(collision.join("keep"), "existing data").unwrap();
        assert!(create_work(&state, &id).is_err());
        assert_eq!(
            std::fs::read_to_string(collision.join("keep")).unwrap(),
            "existing data"
        );
        assert!(!collision.join(".git").exists());
    }

    #[test]
    fn work_creation_rejects_symbolic_links_and_replaced_projects() {
        let root = tempfile::tempdir().unwrap();
        let state = work_state(root.path());
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(state.work_directory.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &state.work_directory).unwrap();
        assert!(create_work(&state, &uuid::Uuid::new_v4().to_string()).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_file(&state.work_directory).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let project = create_work(&state, &id).unwrap();
        std::fs::rename(&project.path, root.path().join("saved-project")).unwrap();
        std::os::unix::fs::symlink(&outside, &project.path).unwrap();
        assert!(create_work(&state, &id).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
    #[test]
    fn profile_session_names_remain_distinct_after_name_normalization() {
        let names = [None, Some("work.a"), Some("work-a"), Some("WORK-A")].map(|profile| {
            session_name("project", profile)
                .replace('.', "-")
                .to_ascii_lowercase()
        });
        for (index, name) in names.iter().enumerate() {
            assert!(!names[..index].contains(name));
        }
        assert_eq!(names[0], "project");
    }

    #[test]
    fn long_profile_session_names_fit_one_filesystem_component() {
        let profile = "x".repeat(64);
        for project in ["p".repeat(200), "🦀".repeat(63)] {
            let name = session_name(&project, Some(&profile));
            assert!(name.len() <= 210, "alias is {} bytes", name.len());
            assert!(name.contains(&profile));
            assert!(name.ends_with(&blake3::hash(profile.as_bytes()).to_hex()[..16]));
        }
    }

    #[tokio::test]
    async fn one_project_reuses_a_separate_session_for_each_profile() {
        let root = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path()).unwrap();
        let config = root.path().join("global.toml");
        std::fs::write(&config, "[profiles.work.settings]\nnetwork='bridge'\n[profiles.review.settings]\nnetwork='offline'\n").unwrap();
        let state = State::open(
            path.clone(),
            Some(config),
            super::super::DaemonOptions::default(),
            "test".into(),
        )
        .unwrap();
        let project = add(&state, path.to_str().unwrap()).unwrap();
        for profile in [None, Some("work"), Some("review")] {
            super::super::sessions::register_fixture(
                &state,
                codex_start_remote::SessionEndpoint {
                    info: codex_start_remote::SessionInfo {
                        id: codex_start_remote::SessionId(profile.unwrap_or("default").into()),
                        name: "project".into(),
                        cwd: project.path.clone(),
                        execution_cwd: project.path.clone(),
                        environment: "default".into(),
                        profile: profile.map(str::to_owned),
                        status: "running".into(),
                        kind: "persistent".into(),
                        capabilities: vec!["codexRpc".into()],
                    },
                    program: String::new(),
                    args: vec![],
                    owner_pid: None,
                },
            );
        }
        for profile in [Some("work"), Some("review"), None, Some("work")] {
            assert_eq!(
                open(&state, &project.id.0, profile).await.unwrap()["session"]["id"],
                profile.unwrap_or("default")
            );
        }
        assert!(open(&state, &project.id.0, Some("missing")).await.is_err());
    }
    #[tokio::test]
    async fn directory_pages_include_folders_and_files_and_hide_dotfiles() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a-folder")).unwrap();
        std::fs::write(root.path().join("b-file"), "test").unwrap();
        std::fs::write(root.path().join(".hidden"), "test").unwrap();
        let first = directories(&json!({"path":root.path(),"limit":1}))
            .await
            .unwrap();
        assert_eq!(first["data"][0]["name"], "a-folder");
        assert_eq!(first["data"][0]["directory"], true);
        let next = directories(&json!({"path":root.path(),"limit":1,"cursor":first["nextCursor"]}))
            .await
            .unwrap();
        assert_eq!(next["data"][0]["name"], "b-file");
        assert!(next["nextCursor"].is_null());
    }
    #[test]
    fn adding_the_same_project_preserves_its_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = State::open(
            root.path().to_owned(),
            None,
            super::super::DaemonOptions::default(),
            "test".into(),
        )
        .unwrap();
        let path = root.path().to_str().unwrap();
        assert_eq!(add(&state, path).unwrap().id, add(&state, path).unwrap().id);
    }
}
