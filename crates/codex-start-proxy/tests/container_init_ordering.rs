use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use codex_start_proxy::container_init::{
    CommandSpec, ExecSpec, InitServiceSpec, InitSpec, TcpForwardServiceSpec,
};

const CLIENT_ENDPOINT: &str = "CODEX_START_TEST_CLIENT_ENDPOINT";
const CLIENT_MARKER: &str = "CODEX_START_TEST_CLIENT_MARKER";

// This test doubles as the argv-only preparation executable used by the
// lifecycle test below. During an ordinary test-harness run the private
// environment variables are absent, so the fixture is a no-op.
#[test]
fn prepare_client_fixture() {
    let Ok(endpoint) = std::env::var(CLIENT_ENDPOINT) else {
        return;
    };
    let marker = std::env::var_os(CLIENT_MARKER).expect("fixture marker must be configured");
    let address: SocketAddr = endpoint.parse().expect("fixture endpoint must be valid");
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .expect("declared init service must accept preparation traffic");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout must be configurable");
    stream
        .write_all(b"ping")
        .expect("fixture request must send");
    let mut response = [0_u8; 4];
    stream
        .read_exact(&mut response)
        .expect("fixture response must arrive");
    assert_eq!(&response, b"pong");
    std::fs::write(marker, b"service-ready").expect("fixture marker must be writable");
}

#[test]
fn declared_services_are_ready_for_prepare_and_stop_when_prepare_fails() {
    let directory = tempfile::tempdir().expect("temporary directory must be available");
    let marker = directory.path().join("prepare-used-service");
    let upstream =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream listener must bind");
    upstream
        .set_nonblocking(true)
        .expect("upstream listener must become nonblocking");
    let upstream_address = upstream.local_addr().expect("upstream address must exist");
    let upstream_thread = thread::spawn(move || serve_one_request(&upstream));
    let service_address = unused_loopback_address();
    let test_binary = std::env::current_exe().expect("test binary path must exist");

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
        prepare: vec![
            CommandSpec {
                program: test_binary.to_string_lossy().into_owned(),
                args: vec![
                    "--exact".to_owned(),
                    "prepare_client_fixture".to_owned(),
                    "--nocapture".to_owned(),
                ],
                env: BTreeMap::from([
                    (CLIENT_ENDPOINT.to_owned(), service_address.to_string()),
                    (
                        CLIENT_MARKER.to_owned(),
                        marker.to_string_lossy().into_owned(),
                    ),
                ]),
                cwd: None,
            },
            CommandSpec {
                program: "/usr/bin/false".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            },
        ],
        services: vec![InitServiceSpec::TcpForward(TcpForwardServiceSpec {
            listen: service_address,
            target: upstream_address.to_string(),
            max_connections: 4,
            connect_timeout_seconds: 2,
            idle_timeout_seconds: 2,
        })],
        command: ExecSpec {
            program: "/usr/bin/true".to_owned().into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
    };
    let spec_path = directory.path().join("init.json");
    std::fs::write(
        &spec_path,
        serde_json::to_vec(&spec).expect("init specification must serialize"),
    )
    .expect("init specification must be writable");

    let output = Command::new(env!("CARGO_BIN_EXE_codex-start-init"))
        .arg("run")
        .arg("--spec")
        .arg(&spec_path)
        .output()
        .expect("init helper must run");

    assert!(!output.status.success(), "second prepare command must fail");
    assert_eq!(
        std::fs::read(&marker).expect("first prepare command must leave its marker"),
        b"service-ready"
    );
    upstream_thread
        .join()
        .expect("upstream fixture must not panic");
    assert!(
        TcpStream::connect_timeout(&service_address, Duration::from_millis(200)).is_err(),
        "service listener must be gone after preparation failure; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn unused_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("ephemeral listener must bind");
    listener
        .local_addr()
        .expect("ephemeral listener address must exist")
}

fn serve_one_request(listener: &TcpListener) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "service never reached upstream");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("upstream accept failed: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("upstream read timeout must be configurable");
    let mut request = [0_u8; 4];
    stream
        .read_exact(&mut request)
        .expect("upstream request must arrive");
    assert_eq!(&request, b"ping");
    stream
        .write_all(b"pong")
        .expect("upstream response must send");
}
