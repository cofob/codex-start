#![cfg(unix)]

use codex_start_proxy::container_init::{CommandSpec, ExecSpec, InitSpec};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn preparation_cannot_consume_or_contaminate_protocol_streams() {
    let directory = tempfile::tempdir().unwrap();
    let spec = InitSpec {
        version: 1,
        uid: None,
        gid: None,
        account: None,
        cwd: None,
        clear_environment: false,
        env: BTreeMap::new(),
        secret_map: None,
        secret_root: directory.path().join("secrets"),
        allow_insecure_secret_permissions: false,
        ownership_paths: Vec::new(),
        ssh: None,
        services: Vec::new(),
        prepare: vec![CommandSpec {
            program: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "if read -r line; then exit 9; fi; echo setup-output; echo setup-error >&2"
                    .to_owned(),
            ],
            env: BTreeMap::new(),
            cwd: None,
        }],
        command: ExecSpec::from_argv(vec!["cat".into()]).unwrap(),
    };
    let path = directory.path().join("spec.json");
    fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-start-init"))
        .args(["run", "--spec"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let protocol = b"{\"id\":1}\n{\"method\":\"initialized\"}\n";
    child.stdin.take().unwrap().write_all(protocol).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, protocol);
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostics.contains("setup-output"));
    assert!(diagnostics.contains("setup-error"));
}
