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
