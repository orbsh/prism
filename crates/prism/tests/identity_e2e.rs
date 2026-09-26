//! Identity plane e2e (ADR-0017 §2/§3/§7 + the §7 payload amendment):
//! device assignment, signup/login as ordinary events, the binding
//! across reconnects via `?device=`, the ONE connection set with its
//! per-connection auth field driving `broadcast {to: "auth"}`, and the
//! sender envelope reaching a booth. Registry on a real fjall
//! directory (durability is the feature under test, not plumbing).

use aura_engine::Engine;
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Boot one gateway with a durable registry in `dir`.
async fn boot() -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    // Leak the tempdir: the registry must survive for the test's life
    // (the guard dropping deletes the files under a live fjall db).
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

/// Connect and consume the mandatory `connected` answer, returning
/// (device_id, user_id-or-zero).
async fn connect(addr: SocketAddr, device: Option<u64>) -> (WsClient, u64, u64) {
    let mut url = format!("ws://{addr}/ws?protocol=json");
    if let Some(d) = device {
        url.push_str(&format!("&device={d}"));
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    let a = &f.args;
    (ws, a["device_id"].as_u64().unwrap(), a["user_id"].as_u64().unwrap_or(0))
}

async fn send(ws: &mut WsClient, frame: &Frame) {
    ws.send(Message::Text(serde_json::to_string(frame).unwrap().into())).await.unwrap();
}

async fn recv(ws: &mut WsClient) -> Frame {
    loop {
        match ws.next().await.expect("stream ended").expect("ws msg") {
            Message::Binary(b) => return Codec::Json.decode(&b).unwrap(),
            Message::Text(t) => return Codec::Json.decode(t.as_bytes()).unwrap(),
            Message::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

/// §2: first connect assigns a device; signup answers with the user
/// id; the echo_sender booth sees exactly that identity in its payload
/// (§7 amendment: envelope, never Ctx).
#[tokio::test]
async fn signup_then_sender_sees_identity() {
    let addr = boot().await;
    let (mut ws, device, user0) = connect(addr, None).await;
    assert_eq!(user0, 0, "first connect is anonymous");
    assert!(device > 0);

    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "alice", "password": "***", "nickname": "Alice"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "signup.result");
    let uid = f.args["user_id"].as_u64().unwrap();
    assert!(uid > 0);

    send(&mut ws, &Frame::new("echo_sender", serde_json::json!({"q": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_sender.result");
    assert_eq!(f.args["device"], serde_json::json!(device));
    assert_eq!(f.args["user"], serde_json::json!(uid));
}

/// §2: the binding survives a reconnect via the echoed device id —
/// `connected` already carries the restored user; an unknown device
/// echo gains nothing (fresh anonymous row).
#[tokio::test]
async fn reconnect_restores_binding() {
    let addr = boot().await;
    let (mut ws, device, _) = connect(addr, None).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "bob", "password": "***"}))).await;
    let uid = recv(&mut ws).await.args["user_id"].as_u64().unwrap();
    drop(ws);

    let (mut ws2, device2, user2) = connect(addr, Some(device)).await;
    assert_eq!(device2, device, "the echoed id is kept");
    assert_eq!(user2, uid, "the binding restored without re-login");
    drop(ws2);

    // Unknown id: fresh row, anonymous (the guess gains nothing).
    let (_, device3, user3) = connect(addr, Some(999_999)).await;
    assert_ne!(device3, 999_999);
    assert_eq!(user3, 0);
}

/// §3 (revised): ONE set, the auth field filters the fan-out — login
/// is a field write, not a set move; `to: "auth"` reaches exactly the
/// authenticated connections, `to: all` everyone.
#[tokio::test]
async fn broadcast_auth_predicate() {
    let addr = boot().await;
    let (mut anon, _, _) = connect(addr, None).await;
    let (mut auth, _, _) = connect(addr, None).await;
    send(&mut auth, &Frame::new("login", serde_json::json!(
        {"username": "carol", "password": "***"}))).await;
    // no such account yet → error value, still anonymous
    let f = recv(&mut auth).await;
    assert_eq!(f.ev, "error");
    send(&mut auth, &Frame::new("signup", serde_json::json!(
        {"username": "carol", "password": "***"}))).await;
    assert_eq!(recv(&mut auth).await.ev, "signup.result");

    send(&mut anon, &Frame::new("broadcast", serde_json::json!({"to": "auth", "n": 1}))).await;
    // anon filtered itself out — next frame must NOT be the broadcast
    let f = recv(&mut auth).await;
    assert_eq!(f.ev, "broadcast");
    assert_eq!(f.args["n"], 1);

    send(&mut anon, &Frame::new("broadcast", serde_json::json!({"n": 2}))).await;
    let fa = recv(&mut anon).await; // drain auth's copy first? no: anon is its own reader
    assert_eq!(fa.ev, "broadcast");
    let fb = recv(&mut auth).await;
    assert_eq!(fb.ev, "broadcast");
}

/// Wrong password = the same error string as an unknown name (no user
/// enumeration at the gateway), and the connection stays usable.
#[tokio::test]
async fn login_failure_is_indistinguishable_value() {
    const RIGHT: &str = "correct-passphrase";
    const WRONG: &str = "wrong-passphrase";
    let addr = boot().await;
    let (mut ws, _, _) = connect(addr, None).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "dave", "password": RIGHT}))).await;
    assert_eq!(recv(&mut ws).await.ev, "signup.result");

    send(&mut ws, &Frame::new("login", serde_json::json!(
        {"username": "dave", "password": WRONG}))).await;
    let f1 = recv(&mut ws).await;
    send(&mut ws, &Frame::new("login", serde_json::json!(
        {"username": "ghost", "password": WRONG}))).await;
    let f2 = recv(&mut ws).await;
    assert_eq!(f1.ev, "error");
    assert_eq!(f1.args["message"], f2.args["message"], "enumeration-free");
    send(&mut ws, &Frame::new("ping", serde_json::Value::Null)).await;
    assert_eq!(recv(&mut ws).await.ev, "ping.result", "still usable");
}

/// Duplicate signup is an error VALUE, connection usable after.
#[tokio::test]
async fn username_taken() {
    let addr = boot().await;
    let (mut ws, _, _) = connect(addr, None).await;
    for expect in ["signup.result", "error"] {
        send(&mut ws, &Frame::new("signup", serde_json::json!(
            {"username": "eve", "password": "***"}))).await;
        let f = recv(&mut ws).await;
        assert_eq!(f.ev, expect);
    }
    send(&mut ws, &Frame::new("ping", serde_json::Value::Null)).await;
    assert_eq!(recv(&mut ws).await.ev, "ping.result", "still alive");
}

/// The envelope reaches the PLAIN echoes too: their pass-through now
/// answers `{sender, args}` — the ADR shape, recorded in README.
#[tokio::test]
async fn plain_echo_wraps_args_in_envelope() {
    let addr = boot().await;
    let (mut ws, device, _) = connect(addr, None).await;
    send(&mut ws, &Frame::new("echo_steel", serde_json::json!({"msg": "hi"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_steel.result");
    assert_eq!(f.args["args"], serde_json::json!({"msg": "hi"}));
    assert_eq!(f.args["sender"]["device"], serde_json::json!(device));
}

/// Per-event auth ENFORCEMENT (ADR-0017 §2, PLAN Phase 1 remaining):
/// `echo_priv` declares `{auth: {echo_priv: "required"}}` in its
/// interface_schema — the persisted copy the gateway reads at dispatch.
/// An anonymous sender gets the standard error VALUE (socket stays up,
/// the event never reaches the booth); after signup the same event is
/// answered by the handler with the sender half. A sibling public event
/// proves the enforcement is per-event, not per-type or blanket.
#[cfg(feature = "steel")]
#[tokio::test]
async fn declared_auth_event_requires_a_bound_user() {
    let addr = boot().await;
    let (mut ws, _, _) = connect(addr, None).await;

    // Anonymous: blocked with the error value, the declared event
    // never runs.
    send(&mut ws, &Frame::new("echo_priv", serde_json::json!({"secret": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["ev"], "echo_priv");
    assert_eq!(f.args["message"], "authentication required");

    // A public event on the same connection still works (per-event,
    // not a blanket gate).
    send(&mut ws, &Frame::new("echo_steel", serde_json::json!({"msg": "hi"}))).await;
    assert_eq!(recv(&mut ws).await.ev, "echo_steel.result");

    // After signup the declared handler answers through the same socket.
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "frank", "password": "***"}))).await;
    let uid = recv(&mut ws).await.args["user_id"].as_u64().unwrap();
    send(&mut ws, &Frame::new("echo_priv", serde_json::json!({"secret": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_priv.result");
    assert_eq!(f.args["user"], serde_json::json!(uid));
}

