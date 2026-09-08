use std::{fs, process::Command};

use serde_json::Value;

#[test]
fn launch_plans_forward_terminal_values_and_leave_unset_values_to_the_image() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, "schema_version = 1\n[settings]\nnetwork = 'offline'\nworktree = 'never'\n[settings.updates]\nenabled = false\n").unwrap();
    for args in [vec!["run"], vec!["shell"], vec!["run", "--tmux"]] {
        for terminal in [Some("screen-256color"), Some(""), None] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
            command
                .current_dir(root.path())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env_remove("CODEX_START_SESSION_WORKER")
                .env_remove("CODEX_START_SESSION_INTERACTIVE")
                .env_remove("TERM")
                .env_remove("COLORTERM")
                .env("TMUX", "/host/tmux/socket,123,0")
                .env("TERMINFO", "/host/terminfo")
                .arg("--config")
                .arg(&config)
                .args(&args)
                .arg("--dry-run");
            if let Some(value) = terminal {
                command
                    .env("TERM", value)
                    .env("COLORTERM", if value.is_empty() { "" } else { "truecolor" });
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
            let env = &plan["runtime"]["env"];
            if terminal == Some("screen-256color") {
                assert_eq!(env["TERM"], "screen-256color");
                assert_eq!(env["COLORTERM"], "truecolor");
            } else {
                assert!(env.get("TERM").is_none());
                assert!(env.get("COLORTERM").is_none());
            }
            assert!(env.get("TMUX").is_none());
            assert!(env.get("TERMINFO").is_none());
        }
    }
}
