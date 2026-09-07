use super::*;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn fixture(
    stream: tokio::io::DuplexStream,
    mut local: mpsc::Receiver<Value>,
    replies: Arc<AtomicU64>,
) {
    let mut socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    let mut first_echo = None;
    loop {
        tokio::select! {
            event = local.recv() => if let Some(event) = event {
                socket.send(Message::Text(event.to_string().into())).await.unwrap();
            } else { break; },
            frame = socket.next() => {
                let Some(Ok(Message::Text(text))) = frame else { break; };
                let message: Value = serde_json::from_str(&text).unwrap();
                let result = match message["method"].as_str() {
                    Some("initialized" | "fixture/hold") => continue,
                    Some("initialize") => json!({}),
                    Some("thread/loaded/list") => json!({"data":["thread-1"]}),
                    Some("thread/read") => json!({"thread":{"id":message["params"]["threadId"],"path":"/fixture/rollout.jsonl","status":{"type":if message["params"]["threadId"] == "thread-1" { "active" } else { "idle" }}}}),
                    Some("thread/resume") => {
                        if message["params"]["threadId"] == "thread-1" {
                            let approval = json!({"id":42,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-1","command":"echo test"}});
                            socket.send(Message::Text(approval.to_string().into())).await.unwrap();
                        }
                        json!({"thread":{"id":message["params"]["threadId"]}})
                    }
                    Some("thread/turns/list") => json!({"data":[]}),
                    Some("fixture/echo") => {
                        let response = json!({"id":message["id"],"result":message["params"]});
                        if let Some(first) = first_echo.take() {
                            socket.send(Message::Text(response.to_string().into())).await.unwrap();
                            socket.send(Message::Text(first)).await.unwrap();
                        } else { first_echo = Some(response.to_string().into()); }
                        continue;
                    }
                    None => {
                        replies.fetch_add(1, Ordering::Relaxed);
                        let event = json!({"method":"serverRequest/resolved","params":{"threadId":"thread-1","requestId":message["id"]}});
                        socket.send(Message::Text(event.to_string().into())).await.unwrap();
                        continue;
                    }
                    Some(method) => panic!("unexpected fixture method {method}"),
                };
                socket.send(Message::Text(json!({"id":message["id"],"result":result}).to_string().into())).await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn shared_backend_keeps_request_ids_and_resolves_replayed_approval_once() {
    let root = tempfile::tempdir().unwrap();
    let state = State::open(
        root.path().to_owned(),
        None,
        super::super::DaemonOptions::default(),
        "fixture".into(),
    )
    .unwrap();
    let endpoint = SessionEndpoint {
        info: SessionInfo {
            id: SessionId::generate(),
            name: "fixture".into(),
            cwd: "/tmp".into(),
            execution_cwd: "/tmp".into(),
            environment: "fixture".into(),
            profile: None,
            status: "running".into(),
            kind: "fixture".into(),
            capabilities: vec!["codexRpc".into()],
        },
        program: String::new(),
        args: vec![],
        owner_pid: None,
    };
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (local, events) = mpsc::channel(8);
    let replies = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(fixture(server, events, replies.clone()));
    let socket = WebSocketStream::from_raw_socket(
        Box::new(client) as codex_start_transport::Stream,
        Role::Client,
        None,
    )
    .await;
    let backend = attach_connection(state.clone(), endpoint, socket)
        .await
        .unwrap();
    state
        .sessions
        .lock()
        .await
        .backends
        .insert(backend.info.id.0.clone(), backend.clone());
    let active = active_tasks(&state).await;
    assert_eq!(active["data"].as_array().unwrap().len(), 1);
    assert_eq!(active["data"][0]["thread"]["id"], "thread-1");
    assert_eq!(active["data"][0]["session"]["name"], "fixture");
    assert_eq!(active["unavailableSessions"], json!([]));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            backend.request("fixture/hold", json!({})),
        )
        .await
        .is_err()
    );
    assert!(backend.pending.lock().unwrap().is_empty());
    // Rejoining the loaded writer restores pending approvals.
    assert!(
        state
            .db(|store| store.events(0, 10))
            .unwrap()
            .iter()
            .any(|e| e.message["id"] == 42)
    );
    let (a, b) = tokio::join!(
        backend.request("fixture/echo", json!({"client":"local"})),
        backend.request("fixture/echo", json!({"client":"phone"}))
    );
    assert_eq!(a.unwrap()["client"], "local");
    assert_eq!(b.unwrap()["client"], "phone");
    let (a, b) = tokio::join!(
        backend.reply(json!(42), json!({"decision":"accept"})),
        backend.reply(json!(42), json!({"decision":"decline"}))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    wait_until(|| {
        replies.load(Ordering::Relaxed) == 1 && backend.approvals.lock().unwrap().is_empty()
    })
    .await;
    assert!(
        backend
            .reply(json!(42), json!({"decision":"accept"}))
            .await
            .is_err()
    );
    // A local client can resolve a later request; Android must then reject its stale card.
    local.send(json!({"id":43,"method":"item/fileChange/requestApproval","params":{"threadId":"thread-1"}})).await.unwrap();
    wait_until(|| backend.approvals.lock().unwrap().contains_key("43")).await;
    local.send(json!({"method":"serverRequest/resolved","params":{"requestId":43,"threadId":"thread-1"}})).await.unwrap();
    wait_until(|| backend.approvals.lock().unwrap().is_empty()).await;
    assert!(
        backend
            .reply(json!(43), json!({"decision":"accept"}))
            .await
            .is_err()
    );
    local.send(json!({"method":"thread/status/changed","params":{"threadId":"thread-2","status":{"type":"active"}}})).await.unwrap();
    wait_until(|| backend.observed.lock().unwrap().contains("thread-2")).await;
    assert_eq!(replies.load(Ordering::Relaxed), 1);
    task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn read_snapshot_routes_to_existing_writer_without_resuming() {
    let root = tempfile::tempdir().unwrap();
    let state = State::open(
        root.path().to_owned(),
        None,
        super::super::DaemonOptions::default(),
        "fixture".into(),
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    for (name, cwd, loaded) in [
        ("reader", "/project", false),
        ("writer", "/project", true),
        ("other", "/other", true),
        ("a-other-profile", "/project", true),
    ] {
        let (outgoing, mut receiver) = mpsc::channel::<Value>(16);
        let info = SessionInfo {
            id: SessionId(name.into()),
            name: name.into(),
            cwd: cwd.into(),
            execution_cwd: cwd.into(),
            environment: "fixture".into(),
            profile: (name == "a-other-profile").then(|| "other".into()),
            status: "running".into(),
            kind: "fixture".into(),
            capabilities: vec![],
        };
        let backend = Arc::new(Backend {
            info: info.clone(),
            endpoint: SessionEndpoint {
                info,
                program: String::new(),
                args: vec![],
                owner_pid: None,
            },
            outgoing,
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            approvals: Mutex::new(BTreeMap::new()),
            observed: Mutex::new(std::collections::BTreeSet::default()),
        });
        state
            .sessions
            .lock()
            .await
            .backends
            .insert(name.into(), backend.clone());
        let seen = seen.clone();
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let method = request["method"].as_str().unwrap();
                seen.lock().unwrap().push((name, method.to_owned()));
                let result = match method {
                    "thread/loaded/list" => {
                        json!({"data":if loaded { vec!["thread-1"] } else { vec![] }})
                    }
                    "thread/turns/list" => json!({"data":[{"id":"turn-1","status":"inProgress"}]}),
                    "thread/items/list" => {
                        json!({"data":[{"item":{"id":"item-1","type":"agentMessage","text":"Working"}}]})
                    }
                    "thread/resume" => json!({"thread":{"id":"thread-1"}}),
                    _ => panic!("unexpected method {method}"),
                };
                let tx = backend
                    .pending
                    .lock()
                    .unwrap()
                    .remove(request["id"].as_str().unwrap())
                    .unwrap();
                tx.send(json!({"result":result})).unwrap();
            }
        });
    }
    let snapshot =
        super::super::thread_access::snapshot(&state, "reader", &json!({"threadId":"thread-1"}))
            .await
            .unwrap();
    assert_eq!(snapshot["sessionId"], "writer");
    assert_eq!(snapshot["turns"]["data"][0]["status"], "inProgress");
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|(name, method)| *name == "other" || *name == "a-other-profile" || method == "thread/resume")
    );
    let access = super::super::server::codex_rpc(&state, &json!({"sessionId":"reader","method":"thread/writeAccess","params":{"threadId":"thread-1"}})).await.unwrap();
    assert_eq!(access["sessionId"], "writer");
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|(_, method)| method == "thread/resume")
    );
    // Once no connected process owns a task, Android can acquire its writer.
    let access = super::super::server::codex_rpc(&state, &json!({"sessionId":"reader","method":"thread/writeAccess","params":{"threadId":"thread-2"}})).await.unwrap();
    assert_eq!(access["sessionId"], "reader");
    assert!(
        seen.lock()
            .unwrap()
            .contains(&("reader", "thread/resume".into()))
    );
}
