//! The resource path, end to end across the three processes that own it (ADR-0048).
//!
//! What is real: aura's engine and its registry, prism's gateway and its frame arms, and a node
//! speaking the effector protocol over the same websocket a real node uses. What is played by hand
//! is only the node's TRANSFER work — it is the half this venue never does, so the test asserts the
//! three things the venue IS responsible for:
//!
//!   1. the node's ask is answered from the venue's registry, with the key minted for that call;
//!   2. a publish records a version and advances the pointer — twice, so the chain is visible;
//!   3. a resource the venue offered read-only is refused a write half by name.
//!
//! The node never names an owner and never names a location: it asks by (kind, seq) and receives
//! material. A test that let it name either would be testing a different design. What TRIGGERS the
//! ask is the script (`ctx.resource`) — that is the node's own business, so this test plays the
//! node and asks directly, the way the host op will.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use aura_booth::InstanceKey;
use effector_protocol::{Frame, ToolCall, ToolResult};
use futures_util::{SinkExt, StreamExt};
use prism::identity::{self, Registry};
use prism::{mounts::MountDecl, Gateway};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_declares_a_resource_and_the_venue_mints_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    // A code base is required for any remote body: the frame carries an ADDRESS, never the bytes
    // (ADR-0027). This test's node does not fetch, so the URL only has to exist in the frame.
    let config = aura_config::EngineConfig {
        code_base_url: Some("http://127.0.0.1:1/code".into()),
        ..Default::default()
    };
    let engine = aura_engine::Engine::start(&config).await.expect("engine boot");
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine.clone(), registry.clone(), prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    // A device and its account: the venue's own rows, which is why the OWNER of a resource is
    // derived here and cannot be claimed by the node.
    // A device exists because this plane minted it — `now()` would only make an id with no row,
    // and a binding to a row that is not there reads back as nobody.
    let device = registry.new_device().unwrap();
    let user = identity::UserId::now();
    registry.bind_login(device, user).unwrap();

    // The mount the turn would call: one type, one node alias.
    let decl = MountDecl {
        type_name: "runner_res".into(),
        node_alias: "res-node".into(),
        carrier: "exec".into(),
        language: "python".into(),
        runner: "python3 -c 'pass'".into(),
        encoding: aura_booth::Encoding::Json,
    };
    let live = prism::mounts::register_all(&engine, std::slice::from_ref(&decl))
        .await
        .unwrap();
    assert_eq!(live, vec!["runner_res".to_string()]);

    // The resource itself: declared in the venue, addressed by (owner, kind, seq).
    let id = aura_realm::resources::Id::new(&user.to_string(), "git", 1).unwrap();
    let registry_handle = engine.realm.lock().await.resources.clone();
    registry_handle
        .secret(&user.to_string(), 1, "gitea-token", "s3cr3t")
        .unwrap();
    registry_handle
        .declare(
            &id,
            &aura_realm::resources::Row {
                desc: "notes".into(),
                source: "https://gitea.s/alice/notes.git".into(),
                at: "main".into(),
                secret: 1,
                secret_name: "gitea-token".into(),
            },
            aura_realm::resources::Mode::ReadWrite,
            identity::now_ms(),
        )
        .unwrap();
    assert_eq!(registry_handle.current(&id).unwrap(), 0, "nothing published yet");

    // The node dials in over the same socket a real one uses.
    let url = effector_protocol::dial_url(
        &format!("ws://127.0.0.1:{}/effector/res-node", addr.port()),
        "res-node-token",
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.expect("dial");
    socket
        .send(Message::Text(
            serde_json::to_string(&Frame::Register {
                node_alias: "res-node".into(),
                carriers: vec!["exec".into()],
            })
            .unwrap(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        next_frame(&mut socket).await,
        Frame::Registered
    ));

    // The turn's dispatch. It carries no declaration: a resource is asked for at RUN time by the
    // script, so the call itself names nothing.
    let device_key = device.to_string();
    let device_text = device_key.clone();
    let owner_text = user.to_string();
    let realm = engine.realm.clone();
    let call = tokio::spawn(async move {
        let target = aura_booth::InstanceId {
            booth_type: "runner_res".into(),
            key: InstanceKey::Named(device_key),
        };
        // The envelope a turn arrives with: the plane authenticated this device, so `sender.user`
        // is what the realm stamps on the activation. Without it the venue has no owner for the
        // call and says so — which is the point of not inferring one from a key.
        let args = json!({
            "code": "print(1)",
            "args": {"n": 1},
            "sender": {"device": device_text, "user": owner_text},
        });
        aura_realm::Realm::call(&realm, None, target, "execute", args).await
    });

    // 1. The node sees its call. The owner the venue stamped at the entrance is what the ask below
    // is answered against — the node never names one.
    let Frame::Call(ToolCall { call_id, .. }) = next_frame(&mut socket).await else {
        panic!("the dispatch must reach the node as a Call");
    };

    // 2. The read half: address + the key, minted per call. The node asks by (kind, seq) only.
    socket
        .send(Message::Text(
            serde_json::to_string(&Frame::MaterialRequest {
                call_id: call_id.clone(),
                kind: "git".into(),
                seq: 1,
                write: false,
            })
            .unwrap(),
        ))
        .await
        .unwrap();
    let material = match next_frame(&mut socket).await {
        Frame::Material {
            url,
            credential,
            mode,
            ..
        } => (url, credential, mode),
        Frame::MaterialFailed { reason, .. } => panic!("the venue refused: {reason}"),
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(material.0, "https://gitea.s/alice/notes.git");
    assert_eq!(material.1.as_deref(), Some("s3cr3t"));
    assert_eq!(material.2, "rw");

    // 3. The publish: a version row, then the pointer. Twice, so the chain is visible.
    let mut published = Vec::new();
    for reference in ["main@abc", "main@def"] {
        socket
            .send(Message::Text(
                serde_json::to_string(&Frame::Synced {
                    call_id: call_id.clone(),
                    kind: "git".into(),
                    seq: 1,
                    reference: reference.into(),
                })
                .unwrap(),
            ))
            .await
            .unwrap();
        // The plane records asynchronously; poll the pointer rather than sleeping a guess. Each
        // publish must MOVE it, so the poll waits for a value that differs from the one before —
        // not merely for a non-zero one, which a slow second write would satisfy.
        let before = published.last().copied().unwrap_or(0);
        let mut current = before;
        for _ in 0..300 {
            current = registry_handle.current(&id).unwrap();
            if current != before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        published.push(current);
    }
    assert!(
        published[1] > published[0],
        "the second publish is a later version: {published:?}"
    );
    assert_eq!(
        registry_handle.current(&id).unwrap(),
        published[1],
        "the pointer names the last publish"
    );

    // The call completes: an answer is what the turn was waiting for.
    socket
        .send(Message::Text(
            serde_json::to_string(&Frame::Result(ToolResult {
                call_id: call_id.clone(),
                outcome: Ok(json!({"done": true, "value": null})),
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
    let answered = call.await.expect("the call task");
    assert!(answered.is_ok(), "the dispatch answered: {answered:?}");

    // A read-only resource must refuse the write half — that is what "a leaked read credential
    // cannot publish" means at this seam.
    let ro = aura_realm::resources::Id::new(&user.to_string(), "s3", 2).unwrap();
    registry_handle
        .declare(
            &ro,
            &aura_realm::resources::Row {
                desc: "inputs".into(),
                source: "s3://alice/in".into(),
                ..Default::default()
            },
            aura_realm::resources::Mode::ReadOnly,
            identity::now_ms(),
        )
        .unwrap();
    let refused = registry_handle.material(&ro, true).unwrap_err().to_string();
    assert!(refused.contains("read-only"), "{refused}");
}


/// The management face, over HTTP: declare, read back, and the pointer it reports. This half needs
/// no node — it is the venue's own table, and it is the only door an operator has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_face_declares_and_reads_a_resource() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine.clone(), registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    let body = json!({
        "owner": "alice", "kind": "git", "seq": 7,
        "desc": "notes", "source": "https://gitea.s/alice/notes.git", "at": "main",
        "mode": "rw",
        "secret": {"name": "gitea-token", "value": "s3cr3t"},
    });
    let (status, answer) = http(addr, "POST", "/admin/resources", Some(body.to_string())).await;
    assert_eq!(status, 201, "the declaration is created: {answer}");
    assert_eq!(answer["rkind"], "git");
    assert_eq!(answer["current"], 0, "nothing is published yet");

    let (status, answer, raw) = http_full(addr, "GET", "/admin/resources/alice/git/7", None).await;
    assert_eq!(status, 200, "the row reads back: {raw}");
    assert_eq!(answer["source"], "https://gitea.s/alice/notes.git");
    assert_eq!(answer["mode"], 1, "rw is stored as the flag the mint checks");
    // The key's VALUE is nowhere in the row: a dumped row hands nobody a credential.
    assert!(!answer.to_string().contains("s3cr3t"), "{answer}");

    let (status, _) = http(addr, "GET", "/admin/resources/alice/git/7", None).await;
    assert_eq!(status, 200, "the same path reads the same row");
    let (status, _) = http(addr, "GET", "/admin/resources/alice/git/8", None).await;
    assert_eq!(status, 404, "an undeclared resource is absent, not an error");
}


async fn http_full(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<String>,
) -> (u16, serde_json::Value, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    let payload = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null), text)
}

/// One request/response over a raw TCP connection — the admin face speaks HTTP/1.1 and closes.
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<String>,
) -> (u16, serde_json::Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let payload = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null))
}

async fn next_frame<S>(socket: &mut S) -> Frame
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within ten seconds")
            .expect("the socket stays open")
            .expect("a readable frame");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("a protocol frame");
        }
    }
}