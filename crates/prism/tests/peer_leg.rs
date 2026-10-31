//! THE PEER LEG SPEAKS THE VIEW VOCABULARY (the turn's exit from this repo).
//!
//! A program is a client like any other: it connects to `/ws`, gets a device, and puts an
//! application event on the plane by naming it with the marker (`ui:turn.submit`). The venue
//! adds nothing and invents no verb; delivery comes back on the same socket as the DATA shape —
//! `{ev: "<stripped name>", args: <payload>}` — because a program reads text, not operations
//! (gravity ADR-0009's two renderings).
//!
//! These tests replace `turn_e2e`: the turn itself lives in gravity, so what is left to prove
//! here is the TRANSPORT — one emit in, one emit out, with the device as the address and the
//! declaration gate in front of it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_booth::{Body, BoothType, ReceiveDecl, RouteResolution};
use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism::{identity::Registry, Deployment, Gateway};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as Ws;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A stub application: it answers `ui:turn.submit` with two `ui:turn.delta` events and one
/// `ui:turn.done`, addressed to the device that asked — the shape gravity's turn emits, without
/// a model or a store behind it.
async fn boot(app: &str) -> (SocketAddr, Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    if app == "gated" {
        // A SCRIPT booth, because a declaration is what gates: the persisted `interface_schema`
        // carries the `auth` block (a Rust body has no schema to persist, so it can never declare
        // one — the same rule that leaves compiled booths without storage).
        engine
            .register(BoothType::script(
                "turn_gate",
                "steel",
                r#"(define (interface_schema args)
  (hash "auth" (hash "turn_gate" "required")
        "receives" (hash "ui:turn.submit" (hash "entry" "turn_gate"))))
(define (turn_gate args) "gated")"#,
            ))
            .await
            .unwrap();
    }
    if app == "stub" {
    let booth = BoothType {
        name: "turn_stub".into(),
        body: Body::Rust(Arc::new(|ctx, args| {
            Box::pin(async move {
                let device = args
                    .get("device")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                for word in ["hel", "lo"] {
                    let mut envelope = json!({ "name": "ui:turn.delta", "data": {"delta": word} });
                    if let Some(device) = &device {
                        envelope["device"] = json!(device);
                    }
                    ctx.emit("ui:turn.delta", envelope).await?;
                }
                let mut done = json!({ "name": "ui:turn.done", "data": {"reply": "hello"} });
                if let Some(device) = &device {
                    done["device"] = json!(device);
                }
                ctx.emit("ui:turn.done", done).await?;
                Ok(Value::Null)
            })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        receives: vec![ReceiveDecl {
            event: "ui:turn.submit".into(),
            entry: None,
            resolution: RouteResolution::Singleton,
            wildcard: false,
        }],
    };
    engine.register(booth).await.unwrap();
    }
    let gw = Gateway::with_deployment(engine, registry, prism::nodes::Posture::Open, Deployment::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

async fn open_peer(addr: SocketAddr) -> WsClient {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("/ws upgrades");
    ws
}

async fn recv_json(ws: &mut WsClient) -> Value {
    loop {
        match ws.next().await.expect("stream").expect("ws msg") {
            Ws::Text(t) => return serde_json::from_str(&t).expect("json frame"),
            Ws::Binary(b) => return serde_json::from_slice(&b).expect("json frame"),
            Ws::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

async fn send_json(ws: &mut WsClient, v: Value) {
    ws.send(Ws::Text(v.to_string().into())).await.expect("send");
}

/// A program's `ui:<name>` event reaches the booth that declared it, and the booth's answer
/// comes back as the same event name minus the marker — the CLI's whole contract in one test.
#[tokio::test]
async fn a_program_submits_a_view_event_and_reads_the_answer_back() {
    let (addr, _gw) = boot("stub").await;
    let mut ws = open_peer(addr).await;
    // The connect frame the peer leg always sends (identity for this socket).
    let first = recv_json(&mut ws).await;
    assert!(first.get("ev").is_some(), "peer leg frames carry `ev`: {first}");

    send_json(
        &mut ws,
        json!({"ev": "ui:turn.submit", "args": {"content": "hi", "channel_id": 1, "member_id": 1}}),
    )
    .await;

    let mut deltas = Vec::new();
    let mut done = None;
    for _ in 0..20 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut ws))
            .await
            .expect("the stub must answer");
        match frame.get("ev").and_then(Value::as_str) {
            Some("turn.delta") => {
                deltas.push(frame["args"]["delta"].as_str().unwrap_or_default().to_string())
            }
            Some("turn.done") => {
                done = Some(frame.clone());
                break;
            }
            _ => {}
        }
    }
    assert_eq!(deltas, vec!["hel".to_string(), "lo".to_string()], "chunks in order");
    let done = done.expect("the answer ends with turn.done");
    assert_eq!(done["args"]["reply"], "hello", "{done}");
}

/// The declaration still gates: an anonymous program gets the standard error value and the socket
/// stays up — a model call is not spent by someone who never logged in.
#[tokio::test]
async fn an_anonymous_program_cannot_put_a_gated_event_on_the_plane() {
    let (addr, _gw) = boot("gated").await;
    let mut ws = open_peer(addr).await;
    let _ = recv_json(&mut ws).await;
    send_json(&mut ws, json!({"ev": "ui:turn.submit", "args": {"content": "hi"}})).await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut ws))
        .await
        .expect("a refusal is an answer, not a silence");
    assert_eq!(frame["ev"], "error", "{frame}");
    assert_eq!(frame["args"]["message"], "authentication required", "{frame}");

    // The socket is still usable: a plain booth call answers as always.
    send_json(&mut ws, json!({"ev": "echo_steel", "args": {"hello": true}})).await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut ws))
        .await
        .expect("the echo answers");
    // A bare `ev` is a CALL to the booth it names, and this leg answers `<ev>.result` (login and
    // signup do the same). A view event a booth EMITS comes back under its own bare name — the
    // two faces differ because one is a call and the other is an announcement.
    assert_eq!(frame["ev"], "echo_steel.result", "{frame}");
}

/// `echo_self` sanity: the stub's own registration did not eat the echo booths' routes.
#[tokio::test]
async fn the_peer_leg_still_dispatches_a_booth_named_by_the_event() {
    let (addr, _gw) = boot("stub").await;
    let mut ws = open_peer(addr).await;
    let _ = recv_json(&mut ws).await;
    send_json(&mut ws, json!({"ev": "echo_steel", "args": {"a": 1}})).await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), recv_json(&mut ws))
        .await
        .expect("echo answers");
    // A bare `ev` is a CALL to the booth it names, and this leg answers `<ev>.result` (login and
    // signup do the same). A view event a booth EMITS comes back under its own bare name — the
    // two faces differ because one is a call and the other is an announcement.
    assert_eq!(frame["ev"], "echo_steel.result", "{frame}");
}