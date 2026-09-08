#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
    engine: PathBuf,
    state: PathBuf,
    name: String,
    container: Value,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let engine = root.path().join("engine");
        fs::write(&engine, include_str!("fixtures/tmux_engine.py")).unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        let state = root.path().join("state.json");
        fs::write(root.path().join("config.toml"), "schema_version = 1\n[settings]\nnetwork = 'offline'\nworktree = 'never'\ntty = 'always'\ntmux = true\n[settings.updates]\nenabled = false\n").unwrap();
        let mut fixture = Self {
            root,
            engine,
            state,
            name: String::new(),
            container: Value::Null,
        };
        let output = fixture
            .command("docker")
            .args(["--name", "existing", "--dry-run"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        fixture.name = plan["container"]["name"].as_str().unwrap().to_owned();
        fixture.container =
            json!({"labels":plan["container"]["labels"], "running":true, "tmux":true});
        fixture.reset(json!({fixture.name.clone(): fixture.container.clone()}));
        fixture
    }

    fn command(&self, runtime: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
        command
            .current_dir(self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("TERM", "screen-256color")
            .env("TMUX_TEST_STATE", &self.state)
            .env_remove("CODEX_START_SESSION_WORKER")
            .env_remove("CODEX_START_SESSION_INTERACTIVE")
            .arg("--config")
            .arg(self.root.path().join("config.toml"))
            .args(["run", "generic", "--runtime", runtime, "--runtime-program"])
            .arg(&self.engine);
        command
    }

    fn reset(&self, containers: Value) {
        fs::write(
            &self.state,
            serde_json::to_vec(&json!({"containers": containers})).unwrap(),
        )
        .unwrap();
    }

    fn calls(&self) -> Vec<Value> {
        let state: Value = serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap();
        state["calls"].as_array().cloned().unwrap_or_default()
    }
}

#[test]
fn tmux_reconnects_before_image_or_worktree_preparation() {
    for runtime in ["docker", "podman"] {
        let fixture = Fixture::new();
        for flags in [vec![], vec!["--name", "existing"]] {
            let output = fixture
                .command(runtime)
                .args(flags)
                .args(["--", "resume", "--last"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                format!("attached {}", fixture.name)
            );
            assert!(!fixture.calls().iter().any(|call| {
                !call.as_array().unwrap().contains(&json!("--help"))
                    && matches!(
                        call[0].as_str(),
                        Some("run" | "pull" | "build" | "rm" | "image")
                    )
            }));
        }
    }
}

#[test]
fn tmux_reconnect_requires_compatible_labels_and_an_unambiguous_session() {
    let fixture = Fixture::new();
    fixture.reset(
        json!({fixture.name.clone():fixture.container.clone(), "other":fixture.container.clone()}),
    );
    let output = fixture.command("docker").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple running tmux sessions"));
    for (label, value) in [
        ("managed", "false"),
        ("project", "other"),
        ("environment", "other"),
        ("home", "other"),
        ("network", "bridge"),
        ("role", "sidecar"),
    ] {
        let mut container = fixture.container.clone();
        container["labels"][format!("cs.fob.wtf.{label}")] = value.into();
        fixture.reset(json!({fixture.name.clone():container}));
        let output = fixture
            .command("docker")
            .args(["--name", "existing"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{label}");
        assert!(!fixture.calls().iter().any(|call| call[0] == "exec"));
    }
}

#[test]
fn tmux_starts_a_new_launch_when_no_live_session_matches() {
    let fixture = Fixture::new();
    for (containers, flags) in [
        (json!({}), vec![]),
        (
            json!({fixture.name.clone(): {"labels":fixture.container["labels"], "running":false, "tmux":false}}),
            vec!["--name", "existing"],
        ),
        (
            json!({fixture.name.clone(): fixture.container.clone()}),
            vec!["--pull"],
        ),
    ] {
        fixture.reset(containers);
        let output = fixture.command("docker").args(flags).output().unwrap();
        // The fixture rejects image operations; reaching one proves the normal launch path was selected.
        assert!(!output.status.success());
        assert!(
            fixture
                .calls()
                .iter()
                .any(|call| call[0] == "image" || call[0] == "pull")
        );
        assert!(
            !fixture
                .calls()
                .iter()
                .any(|call| call.as_array().unwrap().contains(&json!("attach-session")))
        );
    }
}

#[test]
fn tmux_missing_sessions_and_attach_failures_do_not_replace_running_containers() {
    let fixture = Fixture::new();
    let mut container = fixture.container.clone();
    container["tmux"] = false.into();
    fixture.reset(json!({fixture.name.clone():container}));
    let output = fixture
        .command("docker")
        .args(["--name", "existing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no accessible codex-start tmux session")
    );
    let mut container = fixture.container.clone();
    container["attach_status"] = 7.into();
    fixture.reset(json!({fixture.name.clone():container}));
    let output = fixture.command("docker").output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(
        !fixture
            .calls()
            .iter()
            .any(|call| !call.as_array().unwrap().contains(&json!("--help"))
                && matches!(call[0].as_str(), Some("run" | "rm" | "image")))
    );
}
