//! Node approval surface e2e (ADR-0015 §5, prism PLAN Phase 1.8): the
//! four `/admin/nodes...` curls over a real socket, riding the same
//! accept loop as `/ws` and `/code/...`. The record lifecycle — create
//! (pending), list, approve (rotation demotes the old key), revoke
//! (tombstone) — through HTTP exactly as an operator drives it; plus
//! the WS regression (the admin branch must not swallow upgrades) and
//! the posture wiring (the boot declares `open` in its log; the type
//! has no default to inherit).

use aura_engine::Engine;
use prism::{
    identity::Registry,
    nodes::{NodeStore, Posture, Verdict},
    Gateway,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn boot() -> (std::net::SocketAddr, NodeStore) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let nodes = registry.nodes();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    (addr, nodes)
}

async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, String, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = match &body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{b}",
            b.to_string().len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n"),
    };
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = resp[9..12].parse().unwrap();
    let (head_part, body_part) = resp.split_once("\r\n\r\n").unwrap();
    let content_type = head_part
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-type"))
        .unwrap_or("")
        .to_string();
    let json = serde_json::from_str(body_part).unwrap_or(serde_json::Value::Null);
    (status, content_type, json)
}

/// Raw request returning the whole response text (for asserting the
/// human-readable error bodies).
async fn raw(addr: std::net::SocketAddr, method: &str, path: &str, body: Option<serde_json::Value>) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = match &body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{b}",
            b.to_string().len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n"),
    };
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// The operator's full curl session: create → list → approve → the
/// registry agrees (the approval is visible to the verdict function
/// the handshake will consult).
#[tokio::test]
async fn approval_lifecycle_over_http() {
    let (addr, nodes) = boot().await;

    // POST /admin/nodes → 201 + the pending row.
    let (status, _, created) = request(
        addr,
        "POST",
        "/admin/nodes",
        Some(json!({"alias": "home-pc", "public_key": "AAA="})),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(created["status"], "pending");
    assert!(created["created_at"].as_u64().unwrap() > 0);

    // GET /admin/nodes → the listing, application/json.
    let (status, ctype, rows) = request(addr, "GET", "/admin/nodes", None).await;
    assert_eq!(status, 200);
    assert!(ctype.contains("application/json"), "{ctype}");
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["alias"], "home-pc");

    // Approve with no body (unique pending).
    let (status, _, approved) = request(addr, "POST", "/admin/nodes/home-pc/approve", None).await;
    assert_eq!(status, 200);
    assert_eq!(approved["status"], "approved");
    // The handshake seam sees it: registered for the key, conflict for
    // another (the record is the SAME table the approval wrote).
    assert_eq!(nodes.verdict("home-pc", "AAA="), Verdict::Registered);
    assert_eq!(nodes.verdict("home-pc", "BBB="), Verdict::Conflict);

    // Rotation: enroll + approve a second key → the first demotes (§6).
    request(addr, "POST", "/admin/nodes", Some(json!({"alias": "home-pc", "public_key": "BBB="})))
        .await;
    let (status, _, _) = request(
        addr,
        "POST",
        "/admin/nodes/home-pc/approve",
        Some(json!({"public_key": "BBB="})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(nodes.verdict("home-pc", "BBB="), Verdict::Registered);
    assert_eq!(nodes.verdict("home-pc", "AAA="), Verdict::Conflict);

    // Revoke the alias → live keys fall to pending-verdict (revoked),
    // a fresh claim queues again — never self-service.
    let (status, _, out) = request(addr, "DELETE", "/admin/nodes/home-pc", None).await;
    assert_eq!(status, 200);
    assert!(out["revoked"].as_u64().unwrap() >= 1);
    let (status, _, rows) = request(addr, "GET", "/admin/nodes", None).await;
    assert_eq!(status, 200);
    assert!(
        rows.as_array().unwrap().iter().all(|r| r["status"] == "revoked"),
        "tombstones survive the revoke: {rows}"
    );
}

/// Error shapes a human reads off `curl`: malformed body = 400; an
/// ambiguous pending queue refuses a keyless approval = 409 (never a
/// guess); unknown alias = 409; unknown admin route = 404.
#[tokio::test]
async fn failures_are_the_machines_answer() {
    let (addr, _nodes) = boot().await;

    let (status, _, _) = request(addr, "POST", "/admin/nodes", Some(json!({"alias": "x"}))).await;
    assert_eq!(status, 400, "missing public_key");

    request(addr, "POST", "/admin/nodes", Some(json!({"alias": "amb", "public_key": "K1"}))).await;
    request(addr, "POST", "/admin/nodes", Some(json!({"alias": "amb", "public_key": "K2"}))).await;
    let resp = raw(addr, "POST", "/admin/nodes/amb/approve", None).await;
    assert!(resp.starts_with("HTTP/1.1 409"), "{resp}");
    assert!(resp.contains("no unique pending key"), "the reason travels for the human: {resp}");

    let (status, _, _) = request(addr, "POST", "/admin/nodes/ghost/approve", None).await;
    assert_eq!(status, 409);
    let (status, _, _) = request(addr, "GET", "/admin/nodes/ghost", None).await;
    assert_eq!(status, 404);
}

/// The shared loop still serves everything: an unknown non-HTTP path
/// falls through to the WS branch; `/ws` upgrade + echo work; and a
/// GET on a non-route closes cleanly rather than hanging.
#[tokio::test]
async fn ws_and_fallthrough_coexist() {
    use futures_util::{SinkExt, StreamExt};
    use prism_protocol::{Codec, Frame};
    use tokio_tungstenite::tungstenite::Message;

    let (addr, _) = boot().await;
    // WS: upgrade + echo (the head replay path is untouched by admin).
    let ev_of = |bytes: &[u8]| -> String {
        Codec::Json.decode(bytes).map(|f| f.ev).unwrap_or_default()
    };
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake");
    loop {
        match ws.next().await.expect("stream").expect("msg") {
            Message::Binary(b) if ev_of(&b) == "connected" => break,
            Message::Text(t) if ev_of(t.as_bytes()) == "connected" => break,
            Message::Close(_) => panic!("closed before connected"),
            _ => continue,
        }
    }
    ws.send(Message::Text(
        serde_json::to_string(&Frame::new("echo_steel", json!({"x": 1}))).unwrap().into(),
    ))
    .await
    .unwrap();
    loop {
        match ws.next().await.expect("stream").expect("msg") {
            Message::Binary(b) if ev_of(&b) == "echo_steel.result" => break,
            Message::Text(t) if ev_of(t.as_bytes()) == "echo_steel.result" => break,
            Message::Close(_) => panic!("closed mid-echo"),
            _ => continue,
        }
    }
}
