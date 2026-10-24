//! Turn-plane e2e (Phase 1's turn verbs + streaming, gravity's REAL
//! executor): submit opens the iterate stream against gravity's turn
//! booth, tokens stream as `turn.delta` frames (the provider's SSE is
//! stubbed by a local server, the memory seam rides a real mem-server),
//! cancel disposes a live turn, anonymous submit is refused through the
//! SAME interface_schema auth declaration the echo plane enforces, and
//! a duplicate submit while one turn is live is refused. This is the
//! end-to-end chain: WS → gateway pump → gravity turn generator →
//! provider SSE → mem-server append.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The LLM SSE stub: answers every /chat/completions with a streamed
/// reply assembled from a per-request word list (round-robin over
/// SUBMITTED Replies — the token events flow one per chunk, and the
/// PAUSE_MS pacing between chunks keeps the stream observably live for
/// the cancel test). Replies are pushed by tests via `set_reply`.
static REPLIES: std::sync::Mutex<Vec<Vec<String>>> = std::sync::Mutex::new(Vec::new());
static REQ_COUNT: AtomicUsize = AtomicUsize::new(0);

fn set_reply(words: &[&str]) {
    // The stub serves the LAST pushed reply to every request until a
    // new one lands: set_reply REPLACES rather than appends (tests run
    // sequentially in this file, and REQ_COUNT accumulates across
    // them — a round-robin over an append-only list would serve an
    // earlier test's reply once the counter wrapped).
    *REPLIES.lock().unwrap() = vec![words.iter().map(|s| s.to_string()).collect()];
}

async fn sse_server() -> String {
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(|| async {
            let idx = REQ_COUNT.fetch_add(1, Ordering::SeqCst);
            let words = {
                let all = REPLIES.lock().unwrap();
                all.get(idx % all.len().max(1)).cloned().unwrap_or_default()
            };
            // Real SSE pacing: one chunk per token, a few ms between
            // them — the generator's network read blocks on the socket,
            // so the stream is observably live and a mid-turn cancel
            // lands BEFORE the tokens run out.
            let chunks = futures_util::stream::iter(words.into_iter().map(|w| {
                let payload = serde_json::json!({"choices": [{"delta": {"content": format!("{w} ")}}]});
                Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("data: {payload}\n\n")))
            }))
            .chain(futures_util::stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok(axum::body::Bytes::from("data: [DONE]\n\n"))
            }))
            .then(|b| async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                b
            });
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn mem_server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let app = mem_server::open(dir.path().to_path_buf()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::mem::forget(dir); // the store outlives the test
    format!("http://{addr}")
}

async fn boot() -> SocketAddr {
    // The turn deployment facts ride the env (TurnConfig::from_env) —
    // declared here, pointed at the local stubs. The guest binaries are
    // gravity's workspace build (path-adjacent repo, same conventions
    // as the effector fixture: build gravity before running this test).
    let gravity_bins = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../gravity/target/debug"
    );
    std::env::set_var(
        "GRAVITY_PROVIDER_BIN",
        format!("{gravity_bins}/gravity-provider"),
    );
    std::env::set_var("GRAVITY_TURN_BIN", format!("{gravity_bins}/gravity-turn"));
    let endpoint = sse_server().await;
    let store_url = mem_server().await;
    std::env::set_var("GRAVITY_STORE_URL", &store_url);
    std::env::set_var("GRAVITY_ENDPOINT", &endpoint);
    std::env::set_var("GRAVITY_MODEL", "test-model");
    std::env::set_var("GRAVITY_KEY_ENV", "GRAVITY_TEST_KEY");
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");

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

/// The full stream: deltas arrive one per provider token, then done
/// closes; the deltas concatenate to the reply.
#[tokio::test]
async fn turn_submit_streams_provider_tokens() {
    set_reply(&["hel", "lo", "wor", "ld"]);
    let addr = boot().await;
    let mut ws = authed(addr).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "1", "content": "hi"}))).await;
    let mut seen = String::new();
    loop {
        let f = recv(&mut ws).await;
        match f.ev.as_str() {
            "turn.delta" => seen.push_str(f.args["delta"].as_str().unwrap()),
            "turn.done" => break,
            other => panic!("unexpected frame {other}: {}", f.args),
        }
    }
    // The SSE stub appends one space per token (each chunk is a word +
    // its separator); the deltas concatenate to the spaced reply.
    assert_eq!(seen.trim_end(), "hel lo wor ld", "provider tokens streamed as deltas");
}

/// A second submit while one turn is live is refused; after the turn
/// completes the slot is free again.
#[tokio::test]
async fn duplicate_submit_is_refused_and_slot_frees() {
    set_reply(&["a", "b", "c"]);
    let addr = boot().await;
    let mut ws = authed(addr).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "1", "content": "first"}))).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "1", "content": "second"}))).await;
    // The refusal is sent synchronously in dispatch; the first stream's
    // deltas race it on the same socket. Scan until it appears.
    let mut refused = false;
    for _ in 0..6 {
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
        {"session_id": "1", "content": "third"}))).await;
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
        {"session_id": "1", "content": "hi"}))).await;
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
/// submit succeeds). The SSE stub's chunks are paced by the generator's
/// own yield timing — with 30 tokens the stream is observably live.
#[tokio::test]
async fn cancel_disposes_a_live_turn() {
    let words: Vec<&str> = (0..30).map(|_| "w").collect();
    set_reply(&words);
    let addr = boot().await;
    let mut ws = authed(addr).await;
    send(&mut ws, &Frame::new("turn.submit", serde_json::json!(
        {"session_id": "1", "content": "go"}))).await;
    // A couple of deltas arrive...
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "turn.delta");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "turn.delta");
    // ...then cancel.
    send(&mut ws, &Frame::new("turn.cancel", serde_json::json!({}))).await;
    // Both must appear (the dispose races a few deltas at most; cap
    // high because the unbounded channel may have buffered deltas).
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
        {"session_id": "1", "content": "z"}))).await;
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
