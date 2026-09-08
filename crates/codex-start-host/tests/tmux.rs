use std::{fs, process::Command};

use serde_json::Value;

#[test]
fn tmux_plan_uses_a_foreground_terminal_and_rejects_incompatible_modes() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        "schema_version = 1\n[settings]\nnetwork = 'offline'\nworktree = 'never'\n[settings.sessions]\nenabled = true\n",
    )
    .unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
        command
            .current_dir(root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env_remove("CODEX_START_SESSION_WORKER")
            .env_remove("CODEX_START_SESSION_INTERACTIVE")
            .args(["--config"])
            .arg(&config);
        command
    };
    for args in [
        vec!["run", "generic", "--tmux", "--dry-run"],
        vec!["--tmux", "run", "generic", "--dry-run"],
    ] {
        let output = command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["container"]["tty"], true);
        assert_eq!(plan["session"]["enabled"], false);
        assert_eq!(plan["runtime"]["detach"], false);
        let workload = plan["container"]["command"].to_string();
        assert!(workload.contains("tmux"), "{workload}");
        assert!(workload.contains("exec 'codex'"), "{workload}");
        assert!(!workload.contains("app-server"), "{workload}");
    }
    for args in [
        vec!["--tmux", "run", "generic", "--persistent", "--dry-run"],
        vec!["--tmux", "run", "generic", "--no-tty", "--dry-run"],
        vec!["shell", "generic", "--tmux", "--dry-run"],
        vec!["session", "start", "generic", "--tmux", "--dry-run"],
        vec!["run", "generic", "--tmux"],
        vec!["--tmux", "run", "--no-tmux", "--dry-run"],
        vec!["--no-tmux", "run", "--tmux", "--dry-run"],
    ] {
        let output = command().args(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--tmux"));
    }
}

#[test]
fn tmux_config_and_cli_overrides_select_the_launch_mode() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
        command
            .current_dir(root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env_remove("CODEX_START__TMUX")
            .env_remove("CODEX_START_SESSION_WORKER")
            .env_remove("CODEX_START_SESSION_INTERACTIVE")
            .arg("--config")
            .arg(&config);
        command
    };
    for (setting, flags, expected) in [
        ("", vec!["run"], false),
        ("tmux = false", vec!["run", "--tmux"], true),
        ("tmux = true", vec!["run"], true),
        (
            "tmux = false\n[profiles.terminal.settings]\ntmux = true",
            vec!["run", "--profile", "terminal"],
            true,
        ),
        ("tmux = true", vec!["run", "--no-tmux"], false),
        ("tmux = true", vec!["--no-tmux", "run"], false),
        ("tmux = true", vec!["run", "--no-tmux", "--no-tty"], false),
        ("tmux = true", vec!["shell"], false),
        ("tmux = true", vec!["adapter"], false),
        ("tmux = true", vec!["run", "--persistent"], false),
        ("tmux = true", vec!["session", "start"], false),
    ] {
        fs::write(&config, format!("schema_version = 1\n[settings]\nnetwork = 'offline'\nworktree = 'never'\n{setting}\n[settings.sessions]\nenabled = true\n")).unwrap();
        let output = command().args(&flags).arg("--dry-run").output().unwrap();
        assert!(
            output.status.success(),
            "{flags:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        let workload = plan["container"]["command"].to_string();
        assert_eq!(workload.contains("tmux"), expected, "{flags:?}: {workload}");
        if expected {
            assert_eq!(plan["container"]["tty"], true);
            assert_eq!(plan["session"]["enabled"], false);
        }
    }
    let output = command()
        .args(["config", "set", "--global", "tmux", "true"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = command()
        .args(["--output", "json", "config", "show"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(config["tmux"], true);
}
