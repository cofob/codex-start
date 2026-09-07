#![cfg(unix)]

use serde_json::Value;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project with 'quotes'");
        fs::create_dir(&project).unwrap();
        Self {
            _root: temp,
            root,
            project,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-start"));
        command
            .current_dir(&self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env_remove("CODEX_START_CONFIG")
            .env_remove("CODEX_START_SESSION_WORKER")
            .env_remove("CODEX_START_SESSION_INTERACTIVE");
        command
    }

    fn adapter(&self) -> Command {
        let mut command = self.command();
        command
            .args(["adapter", "--project"])
            .arg(&self.project)
            .arg("--offline");
        command
    }

    fn plan(&self, project: &Path) -> Value {
        let output = self
            .command()
            .args(["adapter", "--project"])
            .arg(project)
            .args([
                "--offline",
                "--dry-run",
                "--",
                "app-server",
                "-c",
                "model=\"test\"",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn project_paths_and_client_argv_survive_conflicting_configuration() {
    let fixture = Fixture::new();
    let config = fixture.root.join("explicit.toml");
    fs::write(&config, "schema_version = 1\n[settings]\nworktree = 'always'\ntty = 'always'\nworkdir = '/wrong'\nname = 'shared'\n[settings.sessions]\nenabled = true\n").unwrap();
    let output = fixture
        .adapter()
        .arg("--config")
        .arg(&config)
        .args(["--dry-run", "--", "app-server", "-c", "model=\"test\""])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        plan["runtime"]["workdir"],
        fixture.project.to_str().unwrap()
    );
    assert_eq!(plan["session"]["enabled"], false);
    assert_eq!(plan["container"]["tty"], false);
    assert_eq!(plan["runtime"]["detach"], false);
    assert!(
        plan["container"]["command"]
            .to_string()
            .contains("app-server")
    );
    assert!(
        plan["container"]["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|mount| mount["target"] == fixture.project.to_str().unwrap())
    );
    assert!(!fixture.project.join(".git").exists());
}

#[test]
fn linked_worktree_keeps_its_git_metadata_and_subdirectory_paths() {
    let fixture = Fixture::new();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&fixture.project)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let linked = fixture.root.join("linked");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&fixture.project)
            .args(["worktree", "add", "-qb", "test-linked"])
            .arg(&linked)
            .status()
            .unwrap()
            .success()
    );
    let cwd = linked.join("nested");
    fs::create_dir(&cwd).unwrap();
    let plan = fixture.plan(&cwd);
    assert_eq!(plan["runtime"]["workdir"], cwd.to_str().unwrap());
    let mounts = plan["container"]["mounts"].as_array().unwrap();
    for target in [&linked, &fixture.project.join(".git")] {
        assert!(
            mounts
                .iter()
                .any(|mount| mount["target"] == target.to_str().unwrap()),
            "missing {}",
            target.display()
        );
    }
}

#[test]
fn manual_wrapper_creation_is_not_a_cli_option() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["adapter", "--write-wrapper", "unused"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.root.join("unused").exists());
}

#[test]
fn installed_adapter_finds_its_sibling_and_preserves_native_arguments() {
    let fixture = Fixture::new();
    let installed = fixture.root.join("bin with 'quotes'");
    fs::create_dir(&installed).unwrap();
    let adapter = installed.join("codex-start-adapter");
    fs::copy(env!("CARGO_BIN_EXE_codex-start-adapter"), &adapter).unwrap();
    let launcher = installed.join("codex-start");
    fs::write(
        &launcher,
        b"#!/bin/sh\nprintf '%s\\n' \"$@\"\ncat\nexit 23\n",
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(adapter)
        .current_dir("/")
        .args(["--version", "literal $(value)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        output.stdout,
        b"adapter\n--\n--version\nliteral $(value)\nprotocol\n"
    );
}

#[test]
fn foreground_transport_preserves_bytes_exit_code_and_engine_lifecycle() {
    let fixture = Fixture::new();
    let engine = fixture.root.join("mock-docker");
    fs::write(&engine, r#"#!/bin/sh
if [ "$1" = version ]; then echo 29.0.0; exit 0; fi
if [ "$1" = info ]; then echo '{"SecurityOptions":[]}'; exit 0; fi
case "$*" in *--help*) echo '--add-host --cap-add --cap-drop --label --mount --network --network-alias --read-only --security-opt --userns --internal --alias --filter --format'; exit 0;; esac
case "$1 $2" in
  'image inspect') exit 0;;
  'volume inspect'|'network inspect'|'inspect --format') exit 1;;
  'volume create'|'network create'|'network rm') exit 0;;
esac
if [ "$1" = run ]; then
  printf '%s\n' "$@" > "$MOCK_REQUEST"
  for arg do
    case "$arg" in
      type=bind,src=*,dst=/run/codex-start/init*)
        src=${arg#type=bind,src=}; src=${src%,dst=*}
        cp "$src/spec.json" "$MOCK_SPEC";;
    esac
  done
  cat
  exit 17
fi
exit 99
"#).unwrap();
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = fixture.adapter();
    command
        .args(["--runtime", "docker", "--runtime-program"])
        .arg(&engine)
        .current_dir("/")
        .args(["--", "app-server", "-c", "model=\"literal $(value)\""])
        .env("MOCK_REQUEST", fixture.root.join("request"))
        .env("MOCK_SPEC", fixture.root.join("spec"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let protocol =
        b"{\"id\":1,\"method\":\"initialize\"}\n{\"id\":2,\"params\":\"spaces and \\n\"}\n";
    child.stdin.take().unwrap().write_all(protocol).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, protocol);
    let request = fs::read_to_string(fixture.root.join("request")).unwrap();
    assert!(request.lines().any(|s| s == "--interactive"));
    assert!(request.lines().any(|s| s == "--rm"));
    assert!(!request.lines().any(|s| s == "--tty" || s == "--detach"));
    let spec: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("spec")).unwrap()).unwrap();
    assert_eq!(spec["cwd"], fixture.project.to_str().unwrap());
    assert!(spec["command"].to_string().contains("literal $(value)"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn installed_adapter_routes_multiple_projects_and_restored_threads_from_rpc() {
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader},
        sync::mpsc,
        time::Duration,
    };
    let fixture = Fixture::new();
    let second = fixture.root.join("second");
    fs::create_dir(&second).unwrap();
    let alias = fixture.root.join("project-alias");
    std::os::unix::fs::symlink(&fixture.project, &alias).unwrap();
    let engine = fixture.root.join("mock-docker");
    fs::write(&engine, include_bytes!("fixtures/adapter_engine.py")).unwrap();
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o700)).unwrap();
    let host_codex = fixture.root.join("host-codex");
    fs::copy(&engine, &host_codex).unwrap();
    fs::set_permissions(&host_codex, fs::Permissions::from_mode(0o700)).unwrap();
    let adapter = env!("CARGO_BIN_EXE_codex-start-adapter");
    // Install the mock engine on PATH. All adapter settings come from normal config.
    fs::rename(&engine, fixture.root.join("docker")).unwrap();
    let config = fixture.root.join("adapter.toml");
    fs::write(
        &config,
        "schema_version = 1\n[settings]\nruntime = 'docker'\nnetwork = 'offline'\n[settings.adapter]\nidle_timeout_seconds = 1\n",
    )
    .unwrap();
    let mut template = fixture.command();
    let mut path = vec![fixture.root.clone()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    template
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("CODEX_START_CONFIG", config);
    let probe_cwd = fixture.root.join("probe-cwd");
    let probe = Command::new(adapter)
        .envs(
            template
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("MOCK_PROBE_CWD", &probe_cwd)
        .current_dir("/")
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert_eq!(probe.stdout, b"codex-cli fixture\n");
    assert_ne!(fs::read_to_string(&probe_cwd).unwrap(), "/");
    let mut command = Command::new(adapter);
    command.envs(
        template
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    let diagnostics = fixture.root.join("diagnostics");
    let copy_log = fixture.root.join("attachment-copies");
    let container_fs = fixture.root.join("container-fs");
    let activity = fixture.root.join("container-activity");
    let restored_file = fixture.root.join("historical attachments/archive.bin");
    let restored_folder = fixture.root.join("historical attachments/folder");
    fs::create_dir_all(&restored_folder).unwrap();
    fs::write(&restored_file, [0, 1, 254, 255]).unwrap();
    fs::write(restored_folder.join("nested.txt"), "restored folder").unwrap();
    let sessions = fixture
        .root
        .join("data/codex-start/homes/default/.codex/sessions/2026/09/07");
    fs::create_dir_all(&sessions).unwrap();
    let restored_text = format!(
        "\n# Files pasted by the user:\n\n## archive.bin: {}\n\n## folder: {}/\n\n## My request:\nUse them.\n",
        restored_file.display(),
        restored_folder.display()
    );
    fs::write(
        sessions.join("rollout-2026-09-07T00-00-00-saved-thread.jsonl"),
        format!(
            "{}\n",
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":restored_text}]}})
        ),
    )
    .unwrap();
    let mut child = command
        .current_dir("/")
        .args([
            "-c",
            "features.code_mode_host=true",
            "app-server",
            "--analytics-default-enabled",
        ])
        .env("MOCK_SAVED_CWD", &second)
        .env("MOCK_BACKENDS", fixture.root.join("backends"))
        .env("MOCK_COPIES", &copy_log)
        .env("MOCK_CONTAINER_FS", &container_fs)
        .env(
            "MOCK_RESUME_ATTACHMENTS",
            serde_json::to_string(&[&restored_file, &restored_folder]).unwrap(),
        )
        .env("MOCK_ACTIVITY", &activity)
        .env("CODEX_START_ADAPTER_HOST_CODEX", &host_codex)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&diagnostics).unwrap())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if sender.send(message).is_err() {
                break;
            }
        }
    });
    let receive = || {
        receiver
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|error| {
                panic!("{error}: {}", fs::read_to_string(&diagnostics).unwrap());
            })
    };
    let send = |input: &mut std::process::ChildStdin, value: Value| {
        writeln!(input, "{value}").unwrap();
    };
    send(
        &mut input,
        json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"fixture","version":"1"}}}),
    );
    // VS Code providers can send requests before the initialize response. The
    // adapter must queue local and backend requests until the handshake is
    // complete.
    send(
        &mut input,
        json!({"id":23,"method":"fs/createDirectory","params":{"path":fixture.root.join("pre-init")}}),
    );
    send(
        &mut input,
        json!({"id":24,"method":"skills/list","params":{}}),
    );
    assert_eq!(receive()["id"], 0);
    assert_eq!(receive()["id"], 23);
    assert_eq!(receive()["id"], 24);
    send(&mut input, json!({"method":"initialized"}));
    let backend_count = |event: &str| {
        fs::read_to_string(fixture.root.join("backends"))
            .unwrap()
            .lines()
            .filter(|line| serde_json::from_str::<Value>(line).unwrap()["event"] == event)
            .count()
    };
    // Desktop service directories must not create workspace containers.
    for directory in ["attachments", "visualizations/2026/09/06"] {
        let directory = fixture.root.join(directory);
        send(
            &mut input,
            json!({"id":20,"method":"fs/createDirectory","params":{"path":directory}}),
        );
        assert_eq!(receive()["result"], json!({}));
        send(
            &mut input,
            json!({"id":21,"method":"fs/writeFile","params":{"path":directory.join("image"),"dataBase64":"aGVsbG8="}}),
        );
        assert_eq!(receive()["result"], json!({}));
        send(
            &mut input,
            json!({"id":22,"method":"fs/readFile","params":{"path":directory.join("image")}}),
        );
        assert_eq!(receive()["result"]["dataBase64"], "aGVsbG8=");
    }
    assert_eq!(backend_count("start"), 0);

    send(
        &mut input,
        json!({"id":7,"method":"skills/list","params":{"cwds":[alias,second]}}),
    );
    let skills = receive();
    assert_eq!(skills["id"], 7);
    assert_eq!(skills["result"]["data"].as_array().unwrap().len(), 2);
    for item in skills["result"]["data"].as_array().unwrap() {
        assert_eq!(
            item["skills"][0]["path"],
            "/home/codex/.codex/skills/fixture/SKILL.md"
        );
    }
    assert_eq!(backend_count("start"), 0);
    assert_eq!(backend_count("host-start"), 1);
    let native_start = fs::read_to_string(fixture.root.join("backends"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event"] == "host-start")
        .unwrap();
    assert_eq!(
        Path::new(native_start["codex_home"].as_str().unwrap()),
        fixture.root.join("data/codex-start/homes/default/.codex")
    );
    let native_sqlite_home = native_start["sqlite_home"].as_str().unwrap();
    assert!(native_sqlite_home.contains("control/native-sqlite-home"));
    assert_ne!(
        Path::new(native_sqlite_home),
        fixture.root.join("data/codex-start/homes/default/.codex")
    );

    // Desktop materializes bundled marketplaces on the host. The native
    // catalog backend must read that host path without starting a container.
    send(
        &mut input,
        json!({"id":23,"method":"marketplace/add","params":{"source":fixture.root.join("marketplace")}}),
    );
    assert_eq!(receive()["id"], 23);
    assert_eq!(backend_count("start"), 0);
    assert_eq!(backend_count("host-start"), 1);

    // Loading saved thread state must use one shared control container.
    send(
        &mut input,
        json!({"id":4,"method":"thread/resume","params":{"threadId":"saved-thread"}}),
    );
    assert_eq!(
        receive()["result"]["thread"]["cwd"],
        second.to_str().unwrap()
    );
    send(
        &mut input,
        json!({"id":11,"method":"thread/turns/list","params":{"threadId":"saved-thread"}}),
    );
    assert!(receive().get("result").is_some());
    send(
        &mut input,
        json!({"id":12,"method":"config/read","params":{"cwd":alias}}),
    );
    assert!(receive().get("result").is_some());
    send(
        &mut input,
        json!({"id":13,"method":"hooks/list","params":{"cwds":[alias,second]}}),
    );
    assert!(receive().get("result").is_some());
    assert_eq!(backend_count("start"), 1);

    // A resumed control thread holds the writer lock. Starting project work
    // must stop the control server before the project server resumes it.
    send(
        &mut input,
        json!({"id":25,"method":"turn/start","params":{"threadId":"saved-thread","input":[]}}),
    );
    assert_eq!(receive()["result"]["cwd"], second.to_str().unwrap());
    let copied_path = |path: &Path| container_fs.join(path.strip_prefix(Path::new("/")).unwrap());
    assert_eq!(
        fs::read(copied_path(&restored_file)).unwrap(),
        [0, 1, 254, 255]
    );
    assert_eq!(
        fs::read_to_string(copied_path(&restored_folder.join("nested.txt"))).unwrap(),
        "restored folder"
    );
    assert_eq!(backend_count("start"), 2);
    assert_eq!(backend_count("stop"), 1);

    send(
        &mut input,
        json!({"id":10,"method":"thread/start","params":{}}),
    );
    assert!(receive().get("error").is_some());
    send(
        &mut input,
        json!({"id":1,"method":"thread/start","params":{"cwd":alias}}),
    );
    send(
        &mut input,
        json!({"id":2,"method":"thread/start","params":{"cwd":second}}),
    );
    let mut starts = [receive(), receive()];
    starts.sort_by_key(|value| value["id"].as_u64());
    assert_eq!(
        starts[0]["result"]["thread"]["cwd"],
        fixture.project.to_str().unwrap()
    );
    assert_eq!(
        starts[1]["result"]["thread"]["cwd"],
        second.to_str().unwrap()
    );
    let first_thread = starts[0]["result"]["thread"]["id"].clone();
    // New threads must start in their project servers. A new thread has no
    // rollout file that a second server can resume before the first turn.
    assert_eq!(backend_count("start"), 3);
    send(
        &mut input,
        json!({"id":14,"method":"config/read","params":{"cwd":alias}}),
    );
    assert_eq!(
        receive()["result"]["cwd"],
        fixture.project.to_str().unwrap()
    );
    let control_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while backend_count("stop") == 0 && std::time::Instant::now() < control_deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(backend_count("stop"), 1);
    let attachment_dir = fixture.root.join("attachments");
    send(
        &mut input,
        json!({"id":23,"method":"fs/writeFile","params":{"path":attachment_dir.join("notes.txt"),"dataBase64":"dGV4dCBhdHRhY2htZW50Cg=="}}),
    );
    assert_eq!(receive()["result"], json!({}));
    send(
        &mut input,
        json!({"id":24,"method":"fs/writeFile","params":{"path":attachment_dir.join("archive.bin"),"dataBase64":"AAH+/wo="}}),
    );
    assert_eq!(receive()["result"], json!({}));
    let attachment_folder = attachment_dir.join("folder");
    fs::create_dir(&attachment_folder).unwrap();
    fs::write(attachment_folder.join("nested.txt"), b"nested attachment").unwrap();
    let project_file = fixture.project.join("visible.txt");
    fs::write(&project_file, b"already mounted").unwrap();
    let attachment_text = format!(
        "\n# Files pasted by the user:\n\n## archive.bin: {}\n\n## folder: {}/\n\n## My request:\nliteral $(value)\n",
        attachment_dir.join("archive.bin").display(),
        attachment_folder.display(),
    );
    send(
        &mut input,
        json!({"id":3,"method":"turn/start","params":{"threadId":first_thread,"input":[
            {"type":"text","text":attachment_text},
            {"type":"localImage","path":attachment_dir.join("image")},
            {"type":"localAudio","path":attachment_dir.join("notes.txt")},
            {"type":"mention","name":"notes.txt","path":attachment_dir.join("notes.txt")},
            {"type":"skill","name":"folder","path":attachment_folder},
            {"type":"mention","name":"visible.txt","path":project_file}
        ]}}),
    );
    let turn = receive();
    assert_eq!(turn["result"]["cwd"], fixture.project.to_str().unwrap());
    let forwarded = turn["result"]["params"]["input"].as_array().unwrap();
    for (item, path) in [
        (&forwarded[1], attachment_dir.join("image")),
        (&forwarded[2], attachment_dir.join("notes.txt")),
        (&forwarded[3], attachment_dir.join("notes.txt")),
        (&forwarded[4], attachment_folder.clone()),
    ] {
        assert_eq!(item["path"], path.to_str().unwrap());
    }
    assert_eq!(forwarded[0]["text"], attachment_text);
    assert_eq!(forwarded[5]["path"], project_file.to_str().unwrap());
    assert_eq!(
        turn["result"]["attachmentDataBase64"],
        json!([
            "aGVsbG8=",
            "dGV4dCBhdHRhY2htZW50Cg==",
            "dGV4dCBhdHRhY2htZW50Cg=="
        ])
    );
    assert_eq!(
        turn["result"]["attachmentDirectories"],
        json!([["nested.txt"]])
    );
    assert_eq!(backend_count("start"), 3);
    let copies = fs::read_to_string(&copy_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Vec<String>>(line).unwrap())
        .filter(|args| args.first().is_some_and(|arg| arg == "cp"))
        .collect::<Vec<_>>();
    assert_eq!(copies.len(), 6);
    for source in [
        restored_file,
        restored_folder,
        attachment_dir.join("image"),
        attachment_dir.join("notes.txt"),
        attachment_dir.join("archive.bin"),
        attachment_folder.clone(),
    ] {
        assert!(copies.iter().any(|args| {
            args[1] == source.to_str().unwrap()
                && args[2].split_once(':').map(|(_, path)| path) == source.to_str()
        }));
    }
    assert_eq!(
        fs::read(copied_path(&attachment_dir.join("archive.bin"))).unwrap(),
        [0, 1, 254, 255, 10]
    );
    assert_eq!(
        fs::read_to_string(copied_path(&attachment_folder.join("nested.txt"))).unwrap(),
        "nested attachment"
    );
    send(
        &mut input,
        json!({"id":8,"method":"fs/writeFile","params":{"path":fixture.project.join("new-file"),"dataBase64":"aGVsbG8="}}),
    );
    assert_eq!(receive()["result"], json!({}));
    assert_eq!(
        fs::read(fixture.project.join("new-file")).unwrap(),
        b"hello"
    );
    send(
        &mut input,
        json!({"id":9,"method":"thread/start","params":{"cwd":fixture.root.join("missing")}}),
    );
    let error = receive();
    assert_eq!(error["id"], 9);
    assert!(error.get("error").is_some());
    // Requests from two servers have identical local IDs. Replies must return to their owner.
    send(
        &mut input,
        json!({"id":5,"method":"test/approval","params":{"threadId":first_thread}}),
    );
    send(
        &mut input,
        json!({"id":6,"method":"test/approval","params":{"threadId":"saved-thread"}}),
    );
    let mut approvals = Vec::new();
    for _ in 0..4 {
        let message = receive();
        if message.get("method").is_some() {
            approvals.push(message);
        }
    }
    assert_eq!(approvals.len(), 2);
    assert_ne!(approvals[0]["id"], approvals[1]["id"]);
    for request in &approvals {
        send(
            &mut input,
            json!({"id":request["id"],"result":request["params"]["cwd"]}),
        );
    }
    for _ in 0..2 {
        let approved = receive();
        assert_eq!(approved["method"], "test/approved");
        assert_eq!(approved["params"]["cwd"], approved["params"]["answer"]);
    }
    // Inspection failures and live background processes preserve an unused
    // project backend. Once activity ends, a new full timeout must elapse.
    fs::write(&activity, "error").unwrap();
    send(
        &mut input,
        json!({"id":30,"method":"thread/unsubscribe","params":{"threadId":first_thread}}),
    );
    assert!(receive().get("result").is_some());
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(backend_count("stop"), 1);
    fs::write(&activity, "python3 -m http.server 8080").unwrap();
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(backend_count("stop"), 1);
    fs::remove_file(&activity).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(backend_count("stop"), 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while backend_count("stop") < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(backend_count("start"), 3);
    assert_eq!(backend_count("stop"), 2);
    assert_eq!(
        fs::read_to_string(&diagnostics)
            .unwrap()
            .matches("cannot inspect its processes")
            .count(),
        1
    );
    send(
        &mut input,
        json!({"id":31,"method":"thread/read","params":{"threadId":"later-saved-thread"}}),
    );
    assert!(receive().get("result").is_some());
    send(
        &mut input,
        json!({"id":32,"method":"thread/turns/list","params":{"threadId":"later-saved-thread"}}),
    );
    assert!(receive().get("result").is_some());
    assert_eq!(backend_count("start"), 4);
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn installed_setup_commands_run_without_a_tui_and_restore_editor_settings() {
    let fixture = Fixture::new();
    let settings = fixture.root.join("VS Code/settings.json");
    fs::create_dir(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        "{ // keep me\n \"editor.fontSize\": 14,\n \"chatgpt.cliExecutable\": \"/old/codex\",\n}\n",
    )
    .unwrap();
    let run = |operation: &str| {
        Command::new(env!("CARGO_BIN_EXE_codex-start-adapter"))
            .args([operation, "--vscode-settings"])
            .arg(&settings)
            .env("XDG_DATA_HOME", fixture.root.join("data"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let installed = run("install");
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let contents = fs::read_to_string(&settings).unwrap();
    assert!(contents.contains("// keep me"));
    assert!(contents.contains(env!("CARGO_BIN_EXE_codex-start-adapter")));
    let removed = run("uninstall");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        fs::read_to_string(&settings)
            .unwrap()
            .contains("/old/codex")
    );
    let missing = Command::new(env!("CARGO_BIN_EXE_codex-start-adapter"))
        .args(["install", "--non-interactive"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("select --vscode"));
}
