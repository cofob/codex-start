#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
    engine: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let engine = root.path().join("engine");
        fs::write(&engine, include_str!("fixtures/cache_engine.py")).unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        let state = root.path().join("state.json");
        let volume = |namespace: &str, managed: &str, role: &str, containers: Vec<&str>| {
            json!({"labels": {
                format!("{namespace}.managed"): managed,
                format!("{namespace}.role"): role,
            }, "containers": containers})
        };
        fs::write(&state, serde_json::to_vec(&json!({"volumes": {
            "unused": volume("cs.fob.wtf", "true", "cache", vec![]),
            "legacy": volume("io.codex-start", "true", "cache", vec![]),
            "running": volume("cs.fob.wtf", "true", "cache", vec!["running-container"]),
            "stopped": volume("cs.fob.wtf", "true", "cache", vec!["stopped-container"]),
            "foreign": volume("cs.fob.wtf", "false", "cache", vec![]),
            "data": volume("cs.fob.wtf", "true", "data", vec![]),
            "stale": {"labels": {"cs.fob.wtf.managed":"true", "cs.fob.wtf.role":"data"}, "stale_listing":true},
            "unknown": {"labels": {"cs.fob.wtf.managed":"true", "cs.fob.wtf.role":"cache"}, "inspect_error":true},
        }})).unwrap()).unwrap();
        fs::write(
            root.path().join("config.toml"),
            "schema_version = 1\n[settings.updates]\nenabled = false\n",
        )
        .unwrap();
        Self {
            root,
            engine,
            state,
        }
    }

    fn command(&self, runtime: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
        command
            .current_dir(self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("CACHE_TEST_STATE", &self.state)
            .arg("--config")
            .arg(self.root.path().join("config.toml"))
            .args([
                "--output",
                "json",
                "cache",
                "--runtime",
                runtime,
                "--runtime-program",
            ])
            .arg(&self.engine);
        command
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap()
    }

    fn set_failure(&self, key: &str) {
        let mut state = self.state();
        state["volumes"]["unused"][key] = true.into();
        fs::write(&self.state, serde_json::to_vec(&state).unwrap()).unwrap();
    }
}

#[test]
fn cache_cleanup_preserves_in_use_foreign_and_non_cache_volumes() {
    for runtime in ["docker", "podman"] {
        let fixture = Fixture::new();
        for args in [vec!["list"], vec!["cleanup", "--dry-run"]] {
            let output = fixture.command(runtime).args(&args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            if args[0] == "list" {
                assert_eq!(report["volumes"].as_array().unwrap().len(), 4);
                assert!(
                    report["volumes"]
                        .as_array()
                        .unwrap()
                        .contains(&json!({"name":"stopped", "in_use":true}))
                );
            } else {
                assert_eq!(report["would_remove"], json!(["legacy", "unused"]));
            }
            assert_eq!(fixture.state()["volumes"].as_object().unwrap().len(), 8);
            assert!(
                !fixture.state()["calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|call| call[0] == "volume" && call[1] == "rm")
            );
        }
        let output = fixture.command(runtime).arg("cleanup").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["removed"], json!(["legacy", "unused"]));
        assert_eq!(report["in_use_skipped"], json!(["running", "stopped"]));
        assert_eq!(fixture.state()["volumes"].as_object().unwrap().len(), 6);
        assert!(
            fixture
                .command(runtime)
                .args(["cleanup", "--force"])
                .output()
                .unwrap()
                .status
                .code()
                .is_some_and(|code| code != 0)
        );
    }
}

#[test]
fn cache_cleanup_reports_removal_errors_and_stops_on_failed_usage_inspection() {
    for key in ["remove_error", "probe_error"] {
        let fixture = Fixture::new();
        fixture.set_failure(key);
        let output = fixture.command("docker").arg("cleanup").output().unwrap();
        assert!(!output.status.success());
        assert!(fixture.state()["volumes"]["unused"].is_object());
        if key == "probe_error" {
            assert_eq!(fixture.state()["volumes"].as_object().unwrap().len(), 8);
        } else {
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["errors"][0]["name"], "unused");
        }
    }
}
