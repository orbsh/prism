//! Turn-plane e2e (Phase 1's turn verbs + streaming): submit streams
//! `turn.delta` frames then `turn.done`, cancel disposes a live turn,
//! anonymous submit is refused through the SAME interface_schema auth
//! declaration the echo plane enforces, and a duplicate submit while
//! one turn is live is refused. The stub executor is a python
//! generator — the same shape gravity's real executor takes.

use aura_engine::Engine;
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn boot() -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr) -> WsClient {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    ws
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

/// Sign up on a fresh connection, returning the bound socket.
async fn authed(addr: SocketAddr) -> WsClient {
    let mut ws = connect(addr).await;
    send(&mut ws, &Frame::new("signup", serde_json::json!(
        {"username": "alice", "password": "***"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "signup.result");
    ws
}

/// The full stream: deltas arrive one per word, then done carries the
/// terminal value; the deltas concatenate to the content.
#[tokio::test]
async fn turn_submit_streams_deltas_then_done() {
    let addr = boot().await;
    let mut ws = authed(addr).await;
    let content = "hello streaming turn world";
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": content}))).await;
    let mut seen = String::new();
    loop {
        let f = recv(&mut ws).await;
        match f.ev.as_str() {
            "turn.delta" => seen.push_str(f.args["delta"].as_str().unwrap()),
            "turn.done" => break,
            other => panic!("unexpected frame {other}: {}", f.args),
        }
    }
    assert_eq!(seen.trim_end(), content, "deltas concatenate to the content");
}

/// A second submit while one turn is live is refused; after the turn
/// completes the slot is free again.
#[tokio::test]
async fn duplicate_submit_is_refused_and_slot_frees() {
    let addr = boot().await;
    let mut ws = authed(addr).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": "a b c"}))).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": "x y"}))).await;
    // The refusal is sent synchronously in dispatch; the first stream's
    // deltas race it on the same socket. Scan until it appears.
    let mut refused = false;
    for _ in 0..5 {
        let f = recv(&mut ws).await;
        match f.ev.as_str() {
            "error" => {
                assert_eq!(f.args["ev"], "turn.submit");
                assert!(f.args["message"].as_str().unwrap().contains("already live"));
                refused = true;
                break;
            }
            "turn.delta" => continue,
            other => panic!("unexpected {other}"),
        }
    }
    assert!(refused, "the duplicate submit was refused");
    // Drain to done — the slot freed.
    loop {
        let f = recv(&mut ws).await;
        if f.ev == "turn.done" { break; }
    }
    // A fresh submit now succeeds.
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": "z"}))).await;
    loop {
        let f = recv(&mut ws).await;
        if f.ev == "turn.done" { break; }
    }
}

/// Anonymous submit is refused through the executor's interface_schema
/// `auth` block — the SAME declaration face the echo plane enforces.
#[tokio::test]
async fn anonymous_submit_requires_auth() {
    let addr = boot().await;
    let mut ws = connect(addr).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": "hi"}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["message"], "authentication required");
    // The socket stays up (errors are values, never a close).
    send(&mut ws, &Frame::new("ping", serde_json::json!({}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "ping.result");
}

/// Cancel kills a live turn: turn.cancel.result ok, the pump's pending
/// pull reports the death as turn.error, and the slot frees (a fresh
/// submit succeeds).
#[tokio::test]
async fn cancel_disposes_a_live_turn() {
    let addr = boot().await;
    let mut ws = authed(addr).await;
    // A long content keeps the stub producer mid-stream.
    let content = (0..30).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": content}))).await;
    // A couple of deltas arrive...
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "turn.delta");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "turn.delta");
    // ...then cancel.
    send(&mut ws, &Frame::new("turn.cancel", serde_json::json!({}))).await;
    // Either the cancel.result lands first or the pump's error does;
    // both must appear (the dispose races a few deltas at most — the
    // generator is closed by the session slot lock; cap high because
    // the unbounded channel may have buffered deltas).
    let mut saw_cancel = false;
    let mut saw_error = false;
    for _ in 0..300 {
        let f = recv(&mut ws).await;
        match f.ev.as_str() {
            "turn.cancel.result" => saw_cancel = true,
            "turn.error" => saw_error = true,
            "turn.delta" => continue, // a racing delta before disposal
            other => panic!("unexpected {other}"),
        }
        if saw_cancel && saw_error { break; }
    }
    assert!(saw_cancel && saw_error, "cancel ok + pump-reported death");
    // The slot is free: a fresh submit runs to done.
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "s1", "content": "z"}))).await;
    loop {
        let f = recv(&mut ws).await;
        if f.ev == "turn.done" { break; }
    }
}

/// Cancel with no live turn is idempotent-ok (logout's shape).
#[tokio::test]
async fn cancel_with_no_live_turn_is_idempotent() {
    let addr = boot().await;
    let mut ws = authed(addr).await;
    send(&mut ws, &Frame::new("turn.cancel", serde_json::json!({}))).await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "turn.cancel.result");
    assert_eq!(f.args["ok"], serde_json::json!(true));
}
