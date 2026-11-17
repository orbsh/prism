//! The node handshake on `/effector/<alias>` (PLAN Phase 1.8 step 2; effector ADR-0003).
//!
//! What is locked here is the ORDER and the VERDICT, because both postures share one wire: the
//! node dials `/effector/<alias>?token=…`, the plane hashes the token and — under `required` —
//! asks its registry whether that DIGEST is approved for the alias. The first frame the node
//! sends is a declaration of shape (alias + carriers), never a proof: the authority is the
//! connection, and a tokenless dial is refused where a registry is consulted.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use std::net::SocketAddr;

use aura_engine::Engine;
use effector_protocol::Frame;
use futures_util::{SinkExt, StreamExt};
use prism::identity::Registry;
use prism::nodes::Posture;
use prism::{Deployment, Gateway};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as Ws;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn boot(posture: Posture) -> (SocketAddr, std::sync::Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    let gw = Gateway::with_deployment(engine, registry, posture, Deployment::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

async fn post_json(addr: SocketAddr, path: &str, body: Value) -> String {
    let payload = serde_json::to_string(&body).unwrap();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// Mint a token for `alias` through the operator's face and return the token itself. The token
/// exists in readable form exactly once — in this response (ADR-0003).
async fn mint(addr: SocketAddr, alias: &str) -> String {
    let resp = post_json(addr, "/admin/nodes", serde_json::json!({ "alias": alias })).await;
    assert!(resp.starts_with("HTTP/1.1 201"), "{resp}");
    let body = resp.split_once("\r\n\r\n").expect("a body").1;
    let v: Value = serde_json::from_str(body).expect("json body");
    v["token"]
        .as_str()
        .expect("the mint returns the token")
        .to_string()
}

async fn approve(addr: SocketAddr, alias: &str) {
    let resp = post_json(
        addr,
        &format!("/admin/nodes/{alias}/approve"),
        serde_json::json!({}),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
}

async fn open(addr: SocketAddr, alias: &str, token: Option<&str>) -> WsClient {
    let url = match token {
        Some(t) => format!("ws://{addr}/effector/{alias}?token={t}"),
        None => format!("ws://{addr}/effector/{alias}"),
    };
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("mount upgrades");
    ws
}

async fn recv_frame(ws: &mut WsClient) -> Frame {
    loop {
        match ws.next().await.expect("stream").expect("ws msg") {
            Ws::Text(t) => return serde_json::from_str(&t).expect("frame"),
            Ws::Binary(b) => return serde_json::from_slice(&b).expect("frame"),
            Ws::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

/// Declare shape (the node's only move) and return what the plane decided.
async fn declare(ws: &mut WsClient, alias: &str) -> Frame {
    ws.send(Ws::Text(
        serde_json::to_string(&Frame::Register {
            class: effector_protocol::NodeClass::Host,
            node_alias: alias.to_string(),
            carriers: vec!["nu".into()],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    recv_frame(ws).await
}

/// `required`: an approved token is admitted, and the connection becomes a registered effector.
#[tokio::test]
async fn required_admits_an_approved_token() {
    let (addr, gw) = boot(Posture::Required).await;
    let token = mint(addr, "node1").await;
    approve(addr, "node1").await;

    let mut ws = open(addr, "node1", Some(&token)).await;
    assert!(matches!(declare(&mut ws, "node1").await, Frame::Registered));
    assert!(gw.engine.realm.lock().await.effectors.contains_key("node1"));
}

/// `required`: a token nobody minted does not get in, and the refusal says WHY and how out —
/// the reason would otherwise live only in the host's log, on the other machine.
#[tokio::test]
async fn required_refuses_an_unknown_token() {
    let (addr, _gw) = boot(Posture::Required).await;
    let mut ws = open(addr, "node1", Some("never-minted")).await;
    match declare(&mut ws, "node1").await {
        Frame::Refused { reason } => {
            assert!(
                reason.contains("node1"),
                "the refusal names the alias: {reason}"
            );
            assert!(reason.contains("approve"), "and the way out: {reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// `required`: a second token for an alias that already holds an approved one is a CONFLICT —
/// an enrolled node is never silently replaced.
#[tokio::test]
async fn required_refuses_a_second_token_for_an_enrolled_alias() {
    let (addr, _gw) = boot(Posture::Required).await;
    let _enrolled = mint(addr, "node1").await;
    approve(addr, "node1").await;

    let intruder = mint(addr, "node1").await;
    let mut ws = open(addr, "node1", Some(&intruder)).await;
    match declare(&mut ws, "node1").await {
        Frame::Refused { reason } => {
            assert!(reason.contains("another token"), "{reason}");
            assert!(reason.contains("revoke"), "and the way out: {reason}");
        }
        other => panic!("expected a conflict refusal, got {other:?}"),
    }
}

/// `required`: a dial with no token at all is refused. The authority is the dial, so there is
/// nothing to fall back on — and the refusal names the mint endpoint rather than the symptom.
#[tokio::test]
async fn required_refuses_a_tokenless_dial() {
    let (addr, _gw) = boot(Posture::Required).await;
    let mut ws = open(addr, "node1", None).await;
    match declare(&mut ws, "node1").await {
        Frame::Refused { reason } => {
            assert!(reason.contains("no token"), "{reason}");
            assert!(
                reason.contains("/admin/nodes"),
                "and where to get one: {reason}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// `open` keeps the pre-registry behaviour: whoever can reach the port is admitted, and the
/// registry is not consulted (nothing was enrolled here).
#[tokio::test]
async fn open_admits_a_node_no_registry_knows() {
    let (addr, gw) = boot(Posture::Open).await;
    let mut ws = open(addr, "nobody-enrolled-this", None).await;
    assert!(matches!(
        declare(&mut ws, "nobody-enrolled-this").await,
        Frame::Registered
    ));
    assert!(gw
        .engine
        .realm
        .lock()
        .await
        .effectors
        .contains_key("nobody-enrolled-this"));
}

/// The alias claim is stated twice (path and frame) and both must agree — and the refusal says
/// so on the wire, before the socket goes away. A bare reset would leave the node's operator
/// looking at a closed connection and a host-side log they cannot read.
#[tokio::test]
async fn a_frame_claiming_another_alias_is_refused() {
    let (addr, _gw) = boot(Posture::Open).await;
    let mut ws = open(addr, "node1", None).await;
    match declare(&mut ws, "node2").await {
        Frame::Refused { reason } => {
            assert!(
                reason.contains("node1") && reason.contains("node2"),
                "{reason}"
            );
        }
        other => panic!("an alias mismatch must be refused, got {other:?}"),
    }
}