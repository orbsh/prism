//! Identity plane e2e (ADR-0017 §2/§3/§7 + the 2026-10-29 token split): device assignment,
//! signup/login as ordinary events, the binding across reconnects via `?token=`, the ONE
//! connection set with its per-connection auth field driving `broadcast {to: "auth"}`, and the
//! sender envelope reaching a booth. Registry on a real fjall directory (durability is the feature
//! under test, not plumbing).
//!
//! Ids are uuids, so they travel as TEXT on this wire and are compared as text — the same spelling
//! the browser sees. A token is the only credential: `?device=` restores nothing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What a connect answers with: the identity, and the credential to present next time.
struct Ident {
    device: String,
    user: Option<String>,
    token: String,
}

/// Boot one gateway with a durable registry in `dir`.
async fn boot() -> SocketAddr {
    boot_gw().await.0
}

/// Boot with the gateway handle retained — the server-side surfaces
/// (revocation) are driven in-process, exactly as the admin HTTP face
/// drives them (same `Gateway::revoke_device` entry).
async fn boot_gw() -> (SocketAddr, std::sync::Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    // Leak the tempdir: the registry must survive for the test's life
    // (the guard dropping deletes the files under a live fjall db).
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gw2 = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    (addr, gw2)
}

/// Connect and consume the mandatory `connected` answer.
async fn connect_with(addr: SocketAddr, query: &str) -> (WsClient, Ident) {
    let url = format!("ws://{addr}/ws?protocol=json{query}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    let a = &f.args;
    let ident = Ident {
        device: a["device_id"].as_str().expect("device id is text").to_string(),
        user: a["user_id"].as_str().map(str::to_string),
        token: a["token"].as_str().expect("a token is always issued").to_string(),
    };
    (ws, ident)
}

async fn connect(addr: SocketAddr) -> (WsClient, Ident) {
    connect_with(addr, "").await
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

/// §2: first connect assigns an anonymous device AND a token; signup answers with the user id and a
/// rotated token; the echo_sender booth sees exactly that identity in its payload.
#[tokio::test]
async fn signup_then_sender_sees_identity() {
    let addr = boot().await;
    let (mut ws, ident) = connect(addr).await;
    assert!(ident.user.is_none(), "first connect is anonymous");
    assert_eq!(ident.device.len(), 36, "a uuid's text form");

    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "alice", "password": "correct-horse", "nickname": "Alice"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "signup.result");
    let uid = f.args["user_id"].as_str().expect("user id is text").to_string();
    let token = f.args["token"].as_str().expect("the login rotates the token").to_string();
    assert_ne!(token, ident.token, "a login is a rotation");

    send(&mut ws, &Frame::new("echo_sender", serde_json::json!({"q": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_sender.result");
    assert_eq!(f.args["device"], serde_json::json!(ident.device));
    assert_eq!(f.args["user"], serde_json::json!(uid));
}

/// §2, the token rule: the BINDING survives a reconnect via `?token=`; the device id alone — the
/// thing that used to authenticate — restores nothing.
#[tokio::test]
async fn reconnect_restores_the_binding_through_the_token_only() {
    let addr = boot().await;
    let (mut ws, ident) = connect(addr).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "bob", "password": "correct-horse"}))).await;
    let f = recv(&mut ws).await;
    let uid = f.args["user_id"].as_str().unwrap().to_string();
    let token = f.args["token"].as_str().unwrap().to_string();
    drop(ws);

    let (_, again) = connect_with(addr, &format!("&token={token}")).await;
    assert_eq!(again.device, ident.device, "the token names its device");
    assert_eq!(again.user.as_deref(), Some(uid.as_str()), "restored without re-login");
    assert_eq!(again.token, token, "a live token is handed back as-is");

    // The device id is a NAME now: presenting it authenticates nothing.
    let (_, by_device) = connect_with(addr, &format!("&device={}", ident.device)).await;
    assert_ne!(by_device.device, ident.device, "a device id is not a credential");
    assert!(by_device.user.is_none());

    // An unknown/forged token is the same: a fresh anonymous device.
    let forged = bridge_uuid();
    let (_, by_token) = connect_with(addr, &format!("&token={forged}")).await;
    assert!(by_token.user.is_none());
    assert_ne!(by_token.device, ident.device);
}

/// A uuid4's text form, minted here so the test can present a token no row holds.
fn bridge_uuid() -> String {
    okm_core::Uuid4::new().to_string()
}

/// §3 (revised): ONE set, the auth field filters the fan-out — login
/// is a field write, not a set move; `to: "auth"` reaches exactly the
/// authenticated connections, `to: all` everyone.
#[tokio::test]
async fn broadcast_auth_predicate() {
    let addr = boot().await;
    let (mut anon, _) = connect(addr).await;
    let (mut auth, _) = connect(addr).await;
    send(&mut auth, &Frame::new("login", serde_json::json!(
        {"username": "carol", "password": "correct-horse"}))).await;
    // no such account yet → error value, still anonymous
    let f = recv(&mut auth).await;
    assert_eq!(f.ev, "error");
    send(&mut auth, &Frame::new("signup", serde_json::json!(
        {"username": "carol", "password": "correct-horse"}))).await;
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
    let (mut ws, _) = connect(addr).await;
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
    let (mut ws, _) = connect(addr).await;
    for expect in ["signup.result", "error"] {
        send(&mut ws, &Frame::new("signup", serde_json::json!(
            {"username": "eve", "password": "correct-horse"}))).await;
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
    let (mut ws, ident) = connect(addr).await;
    send(&mut ws, &Frame::new("echo_steel", serde_json::json!({"msg": "hi"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_steel.result");
    assert_eq!(f.args["args"], serde_json::json!({"msg": "hi"}));
    assert_eq!(f.args["sender"]["device"], serde_json::json!(ident.device));
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
    let (mut ws, _) = connect(addr).await;

    // Anonymous: blocked with the error value, the declared event never runs.
    send(&mut ws, &Frame::new("echo_priv", serde_json::json!({"secret": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["ev"], "echo_priv");
    assert_eq!(f.args["message"], "authentication required");

    // A public event on the same connection still works (per-event, not a blanket gate).
    send(&mut ws, &Frame::new("echo_steel", serde_json::json!({"msg": "hi"}))).await;
    assert_eq!(recv(&mut ws).await.ev, "echo_steel.result");

    // After signup the declared handler answers through the same socket.
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "frank", "password": "correct-horse"}))).await;
    let uid = recv(&mut ws).await.args["user_id"].as_str().unwrap().to_string();
    send(&mut ws, &Frame::new("echo_priv", serde_json::json!({"secret": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "echo_priv.result");
    assert_eq!(f.args["user"], serde_json::json!(uid));
}

/// logout: the binding and the live auth field clear TOGETHER, and every token of the device dies —
/// so the token the page holds is no longer a credential. Idempotent: a second logout still answers
/// ok.
#[tokio::test]
async fn logout_clears_binding_and_auth_field() {
    let addr = boot().await;
    let (mut ws, _) = connect(addr).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "grace", "password": "correct-horse"}))).await;
    let f = recv(&mut ws).await;
    let token = f.args["token"].as_str().unwrap().to_string();

    // logout → field cleared: the declared event is blocked again.
    send(&mut ws, &Frame::new("logout", serde_json::json!({}))).await;
    assert_eq!(recv(&mut ws).await.ev, "logout.result");
    send(&mut ws, &Frame::new("echo_priv", serde_json::json!({"secret": 1}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["message"], "authentication required");

    // Idempotent: already anonymous, still ok.
    send(&mut ws, &Frame::new("logout", serde_json::json!({}))).await;
    assert_eq!(recv(&mut ws).await.ev, "logout.result");

    // The credential is dead: reconnecting with it comes back anonymous.
    drop(ws);
    let (_, again) = connect_with(addr, &format!("&token={token}")).await;
    assert!(again.user.is_none(), "a logged-out token restores nothing");
}

/// Server-side revocation: the admin face kills the device's binding and trust, the LIVE connection
/// is kicked with a `revoked` frame, the device's tokens stop working, and a fresh login re-earns
/// the device.
#[tokio::test]
async fn revocation_kicks_connection_and_kills_trust() {
    let (addr, gw) = boot_gw().await;
    let (mut ws, ident) = connect(addr).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "malory", "password": "correct-horse"}))).await;
    let f = recv(&mut ws).await;
    let uid = f.args["user_id"].as_str().unwrap().to_string();
    let token = f.args["token"].as_str().unwrap().to_string();

    // Server-side kick: the connection receives the revocation frame, then the socket closes.
    let device: prism::identity::DeviceId = ident.device.parse().unwrap();
    let kicked = gw.revoke_device(device).unwrap();
    assert_eq!(kicked, 1, "one live connection");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "revoked");
    assert_eq!(f.args["device_id"], serde_json::json!(ident.device));
    // The socket is (or immediately will be) closing: the next read ends the stream (Close) — recv
    // panics on Close, so read raw.
    loop {
        match ws.next().await {
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Binary(b))) => {
                let f: Frame = serde_json::from_slice(&b).unwrap();
                assert!(f.ev != "revoked", "no second revocation frame");
            }
            Some(Ok(Message::Text(t))) => {
                let f: Frame = serde_json::from_slice(t.as_bytes()).unwrap();
                assert!(f.ev != "revoked", "no second revocation frame");
            }
            _ => continue,
        }
    }

    // The token died with the device: no restore, and the device row is stamped.
    drop(ws);
    let (mut ws2, anon) = connect_with(addr, &format!("&token={token}")).await;
    assert!(anon.user.is_none(), "a revoked device's token restores nothing");
    assert_ne!(anon.device, ident.device);

    // Fresh login re-earns the device: the auth field lives again, with a NEW token.
    send(&mut ws2, &Frame::new("login", serde_json::json!(
        {"username": "malory", "password": "correct-horse"}))).await;
    let f = recv(&mut ws2).await;
    assert_eq!(f.ev, "login.result");
    assert_eq!(f.args["user_id"], serde_json::json!(uid));
    assert_ne!(f.args["token"], serde_json::json!(token), "the token rotated");
    send(&mut ws2, &Frame::new("echo_priv", serde_json::json!({"n": 1}))).await;
    let f = recv(&mut ws2).await;
    assert_eq!(f.ev, "echo_priv.result", "re-login re-earns the auth field");

    // Unknown device: refused decision.
    assert!(gw.revoke_device(okm_core::Uuid7::now()).is_err());
}