//! Launcher settings use host-selected paths, schema validation, and revision checks.

use super::{error, state::State};
use crate::{
    configuration::{ConfigContext, ConfigTarget},
    error::Result,
    paths::{atomic_write, ensure_regular_file_or_missing},
};
use codex_start_core::{ConfigDocument, ConfigLayer, ConfigLayerKind, ConfigPatch, ConfigResolver};
use serde_json::{Value, json};
use std::{io::Read, path::Path, sync::Arc};

const LIMIT: usize = 256 * 1024;

fn read(path: &Path) -> Result<Option<String>> {
    if !ensure_regular_file_or_missing(path)? {
        return Ok(None);
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .map_err(error)?
        .take((LIMIT + 1) as u64)
        .read_to_string(&mut text)
        .map_err(error)?;
    if text.len() > LIMIT {
        return Err(error("settings file exceeds 256 KiB"));
    }
    Ok(Some(text))
}

fn version(text: Option<&str>) -> String {
    text.map_or_else(
        || "missing".into(),
        |text| blake3::hash(text.as_bytes()).to_hex().to_string(),
    )
}

pub async fn dispatch(state: &Arc<State>, method: &str, params: &Value) -> Result<Value> {
    let _guard = state.settings_edits.lock().await;
    let cwd = if let Some(id) = params["projectId"].as_str() {
        std::path::PathBuf::from(super::projects::get(state, id)?.path)
    } else {
        state.root.clone()
    };
    let config = state.config.clone();
    let method = method.to_owned();
    let params = params.clone();
    tokio::task::spawn_blocking(move || {
        let context = ConfigContext::discover_at(config.as_deref(), &cwd)?;
        edit(&context, &method, &params)
    })
    .await
    .map_err(error)?
}

fn edit(context: &ConfigContext, method: &str, params: &Value) -> Result<Value> {
    let target = match params["scope"].as_str().unwrap_or("global") {
        "global" | "profile" => ConfigTarget::Global,
        "project" if params["projectId"].is_string() => ConfigTarget::Project,
        _ => {
            return Err(error(
                "select global settings, a profile, or a registered project",
            ));
        }
    };
    let path = context.config_path(target);
    let original = read(path)?;
    let contents = original.as_deref().unwrap_or("schema_version = 1\n");
    let profile = (params["scope"] == "profile")
        .then(|| params["profile"].as_str())
        .flatten();
    if params["scope"] == "profile" && profile.is_none_or(str::is_empty) {
        return Err(error("enter a profile name"));
    }
    if method == "launcher/list" {
        let document = ConfigDocument::parse_file(path, contents).map_err(error)?;
        return Ok(json!({"profiles":document.profiles.keys().collect::<Vec<_>>()}));
    }
    if method == "launcher/read" {
        let text = if let Some(name) = profile {
            let document = ConfigDocument::parse_file(path, contents).map_err(error)?;
            document.profiles.get(name).map_or_else(
                || Ok("[settings]\n".to_owned()),
                |value| toml::to_string_pretty(value).map_err(error),
            )?
        } else {
            contents.to_owned()
        };
        return Ok(json!({"path":path,"text":text,"version":version(original.as_deref())}));
    }
    let text = params["text"]
        .as_str()
        .ok_or_else(|| error("missing settings text"))?;
    if text.len() > LIMIT {
        return Err(error("settings file exceeds 256 KiB"));
    }
    if params["version"].as_str() != Some(version(original.as_deref()).as_str()) {
        return Err(error("settings changed on the host; reload before saving"));
    }
    let rendered = if let Some(name) = profile {
        let mut document = contents.parse::<toml_edit::DocumentMut>().map_err(error)?;
        let profile = text.parse::<toml_edit::DocumentMut>().map_err(error)?;
        if !document.contains_key("profiles") {
            document["profiles"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let profiles = document["profiles"]
            .as_table_like_mut()
            .ok_or_else(|| error("invalid profiles table"))?;
        profiles.insert(name, toml_edit::Item::Table(profile.as_table().clone()));
        document.to_string()
    } else {
        text.to_owned()
    };
    if rendered.len() > LIMIT {
        return Err(error("settings file exceeds 256 KiB"));
    }
    validate(context, target, &rendered)?;
    if read(path)? != original {
        return Err(error("settings changed on the host; reload before saving"));
    }
    atomic_write(path, &rendered)?;
    // Keep settings contents out of the request outcome journal.
    Ok(json!({"version":version(Some(&rendered))}))
}

fn validate(context: &ConfigContext, target: ConfigTarget, rendered: &str) -> Result<()> {
    let document = ConfigDocument::parse_file(context.config_path(target), rendered).map_err(error)?;
    if target == ConfigTarget::Project {
        document.validate_as_project().map_err(error)?;
    }
    let global = if target == ConfigTarget::Global {
        document.clone()
    } else {
        ConfigDocument::parse_file(
            &context.global_file,
            read(&context.global_file)?
                .as_deref()
                .unwrap_or("schema_version = 1\n"),
        )
        .map_err(error)?
    };
    // Resolve every profile, including unused profiles, to reject missing parents and cycles.
    for selected in
        std::iter::once(None).chain(global.profiles.keys().map(|name| Some(name.clone())))
    {
        let mut resolver = ConfigResolver::new();
        resolver
            .add_document(
                ConfigLayerKind::BuiltIn,
                "discovered homes",
                ConfigDocument {
                    homes: crate::home::discover_home_configs(&context.paths)?,
                    ..Default::default()
                },
            )
            .map_err(error)?;
        resolver
            .add_document(ConfigLayerKind::Global, "global", global.clone())
            .map_err(error)?;
        if target == ConfigTarget::Project {
            resolver
                .add_document(ConfigLayerKind::Project, "project", document.clone())
                .map_err(error)?;
        }
        if let Some(selected) = selected {
            resolver
                .add_layer(ConfigLayer::new(
                    ConfigLayerKind::CommandLine,
                    "profile",
                    ConfigPatch {
                        profile: Some(selected),
                        ..Default::default()
                    },
                ))
                .map_err(error)?;
        }
        resolver.resolve().map_err(error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &Path) -> ConfigContext {
        let context = ConfigContext {
            paths: crate::paths::AppPaths {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
            },
            cwd: root.to_owned(),
            repo: None,
            global_file: root.join("global.toml"),
            project_file: root.join("project.toml"),
        };
        context.paths.ensure().unwrap();
        context
    }

    fn save(context: &ConfigContext, target: Value, text: &str) -> Result<Value> {
        let current = edit(context, "launcher/read", &target)?;
        let mut params = target;
        params["text"] = json!(text);
        params["version"] = current["version"].clone();
        edit(context, "launcher/write", &params)
    }

    #[test]
    fn profiles_and_project_settings_preserve_other_layers_and_reject_invalid_changes() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        save(
            &context,
            json!({"scope":"global"}),
            "# Keep this comment\n[settings]\nnetwork='bridge'\n",
        )
        .unwrap();
        save(
            &context,
            json!({"scope":"profile","profile":"work"}),
            "[settings]\nnetwork='offline'\n",
        )
        .unwrap();
        save(
            &context,
            json!({"scope":"profile","profile":"review"}),
            "extends='work'\n[settings]\nrebuild=true\n",
        )
        .unwrap();
        assert_eq!(
            edit(&context, "launcher/list", &json!({})).unwrap()["profiles"],
            json!(["review", "work"])
        );
        let global = read(&context.global_file).unwrap().unwrap();
        assert!(global.starts_with("# Keep this comment"));
        assert!(
            save(
                &context,
                json!({"scope":"profile","profile":"work"}),
                "extends='review'\n[settings]\n"
            )
            .is_err()
        );
        assert!(
            save(
                &context,
                json!({"scope":"profile","profile":"missing-parent"}),
                "extends='missing'\n[settings]\n"
            )
            .is_err()
        );
        assert!(
            save(
                &context,
                json!({"scope":"profile","profile":"bad/name"}),
                "[settings]\n"
            )
            .is_err()
        );
        assert!(
            save(
                &context,
                json!({"scope":"profile","profile":"work"}),
                "[settings.updates]\nenabled=true\n"
            )
            .is_err()
        );
        assert_eq!(read(&context.global_file).unwrap().unwrap(), global);
        let project = json!({"scope":"project","projectId":"registered"});
        save(&context, project.clone(), "[settings]\nprofile='review'\n").unwrap();
        assert!(
            save(
                &context,
                project.clone(),
                "[profiles.other.settings]\nrebuild=true\n"
            )
            .is_err()
        );
        assert!(save(&context, project, "[settings]\nprofile='unknown'\n").is_err());
        assert_eq!(
            read(&context.project_file).unwrap().unwrap(),
            "[settings]\nprofile='review'\n"
        );
    }

    #[test]
    fn stale_edits_and_symlinks_do_not_replace_settings() {
        let root = tempfile::tempdir().unwrap();
        let context = context(root.path());
        let snapshot = edit(&context, "launcher/read", &json!({"scope":"global"})).unwrap();
        std::fs::write(&context.global_file, "# changed elsewhere\n").unwrap();
        assert!(edit(&context, "launcher/write", &json!({"scope":"global", "version":snapshot["version"], "text":"[settings]\nrebuild=true\n"})).is_err());
        assert_eq!(
            read(&context.global_file).unwrap().unwrap(),
            "# changed elsewhere\n"
        );
        std::os::unix::fs::symlink(&context.global_file, &context.project_file).unwrap();
        assert!(
            edit(
                &context,
                "launcher/read",
                &json!({"scope":"project","projectId":"registered"})
            )
            .is_err()
        );
        assert!(
            edit(
                &context,
                "launcher/read",
                &json!({"scope":"project","path":"/tmp"})
            )
            .is_err()
        );
    }
}
