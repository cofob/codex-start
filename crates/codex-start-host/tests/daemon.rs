#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
    command
        .args(["--output", "json"])
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("CODEX_START_CONFIG")
        .stdin(Stdio::null());
    command
}

#[test]
fn daemon_process_starts_with_private_socket_and_stops_through_local_control() {
    // macOS limits Unix socket paths to 104 bytes, including the terminator.
    let root = tempfile::Builder::new()
        .prefix("csd-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut child = Daemon(
        command(root.path())
            .args(["daemon", "run", "--no-yggdrasil", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = command(root.path())
            .args(["daemon", "status"])
            .output()
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if status["status"] == "running" {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "daemon exited before readiness"
        );
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }
    let socket = root
        .path()
        .join("data/codex-start/remote/default/control.sock");
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let invitation = command(root.path()).args(["connect"]).output().unwrap();
    assert!(invitation.status.success());
    let value: serde_json::Value = serde_json::from_slice(&invitation.stdout).unwrap();
    assert!(
        value["uri"]
            .as_str()
            .unwrap()
            .starts_with(codex_start_remote::INVITATION_PREFIX)
    );
    let stop = command(root.path())
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(stop.status.success());
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!socket.exists());
}

#[test]
fn daemon_restart_preserves_settings_and_identity_and_starts_when_stopped() {
    struct Cleanup<'a>(&'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = command(self.0).args(["daemon", "stop"]).output();
        }
    }
    let root = tempfile::Builder::new()
        .prefix("csr-")
        .tempdir_in("/tmp")
        .unwrap();
    let _cleanup = Cleanup(root.path());
    let run = |args: &[&str]| {
        let output = command(root.path()).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    run(&[
        "daemon",
        "start",
        "--no-yggdrasil",
        "--bind",
        "127.0.0.1:0",
        "--advertise-host",
        "127.0.0.1",
    ]);
    let directory = root.path().join("data/codex-start/remote/default");
    let options = std::fs::read(directory.join("options.json")).unwrap();
    let invitation = run(&["connect"]);
    assert_eq!(run(&["daemon", "restart"])["status"], "running");
    assert_eq!(
        std::fs::read(directory.join("options.json")).unwrap(),
        options
    );
    assert_eq!(run(&["connect"])["uri"], invitation["uri"]);
    assert_eq!(run(&["daemon", "status"])["yggdrasil"], "disabled");
    run(&["daemon", "stop"]);
    assert_eq!(run(&["daemon", "restart"])["status"], "running");
    assert_eq!(
        std::fs::read(directory.join("options.json")).unwrap(),
        options
    );
}
