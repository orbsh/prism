//! The node handshake on `/effector/<alias>` (PLAN Phase 1.8 step 2; effector ADR-0002).
//!
//! What is locked here is the ORDER and the VERDICT, because both postures now share one wire:
//! the host speaks first with a per-connection nonce, the node answers with a signed `Register`,
//! and only `required` consults the registry. `open` means "no registry", never "no proof" — a
//! wrong signature is refused in both.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use std::net::SocketAddr;

use aura_engine::Engine;
use effector_protocol::{Frame, SigningKey};
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
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    let gw = Gateway::with_deployment(engine, registry, posture, Deployment::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

/// A node key on disk: the handshake proves possession, so every client needs one.
fn node_key(tag: &str) -> SigningKey {
    let mut path = std::env::temp_dir();
    path.push(format!("prism-handshake-{tag}-{}.key", std::process::id()));
    let _ = std::fs::remove_file(&path);
    effector_protocol::write_keypair(&path).unwrap();
    effector_protocol::read_signing_key(&path).unwrap()
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

async fn open(addr: SocketAddr, alias: &str) -> WsClient {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/effector/{alias}"))
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

/// Answer the challenge the host is required to speak first, and return what it decided.
async fn handshake(ws: &mut WsClient, key: &SigningKey, alias: &str, claimed: Option<&SigningKey>) -> Frame {
    let nonce = match recv_frame(ws).await {
        Frame::Challenge { nonce } => nonce,
        other => panic!("the host must speak first, got {other:?}"),
    };
    // `claimed` is the key the frame ADVERTISES; signing with `key` is what the host checks, so
    // passing a different one is how a forged claim looks on the wire.
    let claimed = claimed.unwrap_or(key);
    ws.send(Ws::Text(
        serde_json::to_string(&Frame::Register {
            node_alias: alias.to_string(),
            public_key: effector_protocol::public_key_b64(claimed),
            signature: effector_protocol::sign_register(key, &nonce, alias),
            carriers: vec!["nu".into()],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    recv_frame(ws).await
}

/// `required`: an approved key is admitted, and the connection becomes a registered effector.
#[tokio::test]
async fn required_admits_an_approved_key() {
    let (addr, gw) = boot(Posture::Required).await;
    let key = node_key("approved");
    let public_key = effector_protocol::public_key_b64(&key);

    // The operator's queue: the claim is created then approved, both over the curl-first faces.
    let created = post_json(addr, "/admin/nodes", serde_json::json!({"alias": "node1", "public_key": public_key})).await;
    assert!(created.starts_with("HTTP/1.1 201"), "{created}");
    let approved = post_json(addr, "/admin/nodes/node1/approve", serde_json::json!({})).await;
    assert!(approved.starts_with("HTTP/1.1 200"), "{approved}");

    let mut ws = open(addr, "node1").await;
    assert!(matches!(handshake(&mut ws, &key, "node1", None).await, Frame::Registered));
    // Admitted for real: the alias is in the realm's effector table.
    assert!(gw.engine.realm.lock().await.effectors.contains_key("node1"));
}

/// `required`: an unenrolled key does not get in, and it says WHY — the reason would otherwise
/// exist only in the host's log, on the other machine.
#[tokio::test]
async fn required_refuses_an_unapproved_key() {
    let (addr, _gw) = boot(Posture::Required).await;
    let key = node_key("stranger");
    let mut ws = open(addr, "node1").await;
    match handshake(&mut ws, &key, "node1", None).await {
        Frame::Refused { reason } => {
            assert!(reason.contains("node1"), "the refusal names the alias: {reason}");
            assert!(reason.contains("approve"), "and the way out: {reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// `required`: an enrolled alias whose approved key differs is a CONFLICT — an enrolled key is
/// never silently replaced.
#[tokio::test]
async fn required_refuses_a_different_key_for_an_enrolled_alias() {
    let (addr, _gw) = boot(Posture::Required).await;
    let enrolled = node_key("enrolled");
    let public_key = effector_protocol::public_key_b64(&enrolled);
    post_json(addr, "/admin/nodes", serde_json::json!({"alias": "node1", "public_key": public_key})).await;
    let approved = post_json(addr, "/admin/nodes/node1/approve", serde_json::json!({})).await;
    assert!(approved.starts_with("HTTP/1.1 200"), "{approved}");

    let intruder = node_key("intruder");
    let mut ws = open(addr, "node1").await;
    match handshake(&mut ws, &intruder, "node1", None).await {
        Frame::Refused { reason } => {
            assert!(reason.contains("another key"), "{reason}");
            assert!(reason.contains("revoke"), "and the way out: {reason}");
        }
        other => panic!("expected a conflict refusal, got {other:?}"),
    }
}

/// A signature that does not match the key the frame advertises is refused in BOTH postures:
/// `open` means "no registry", never "no proof".
#[tokio::test]
async fn a_forged_signature_is_refused_in_both_postures() {
    for posture in [Posture::Open, Posture::Required] {
        let (addr, _gw) = boot(posture).await;
        let signer = node_key("signer");
        let claimed = node_key("claimed");
        let mut ws = open(addr, "node1").await;
        match handshake(&mut ws, &signer, "node1", Some(&claimed)).await {
            Frame::Refused { reason } => {
                assert!(reason.contains("signature"), "{posture:?}: {reason}");
            }
            other => panic!("{posture:?}: expected a refusal, got {other:?}"),
        }
    }
}

/// `open` keeps the pre-registry behaviour: any key that proves itself is admitted, and the
/// registry is not consulted (nothing was enrolled here).
#[tokio::test]
async fn open_admits_a_key_no_registry_knows() {
    let (addr, gw) = boot(Posture::Open).await;
    let key = node_key("anonymous-node");
    let mut ws = open(addr, "nobody-enrolled-this").await;
    assert!(matches!(handshake(&mut ws, &key, "nobody-enrolled-this", None).await, Frame::Registered));
    assert!(gw.engine.realm.lock().await.effectors.contains_key("nobody-enrolled-this"));
}

/// The alias claim is stated twice (path and frame) and both must agree — a path that admitted
/// any alias would make the URL a lie in every log line that follows.
#[tokio::test]
async fn a_frame_claiming_another_alias_is_refused() {
    let (addr, _gw) = boot(Posture::Open).await;
    let key = node_key("mismatch");
    let mut ws = open(addr, "node1").await;
    let nonce = match recv_frame(&mut ws).await {
        Frame::Challenge { nonce } => nonce,
        other => panic!("expected a challenge, got {other:?}"),
    };
    ws.send(Ws::Text(
        serde_json::to_string(&Frame::Register {
            node_alias: "node2".into(),
            public_key: effector_protocol::public_key_b64(&key),
            signature: effector_protocol::sign_register(&key, &nonce, "node2"),
            carriers: vec![],
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    // The mount bails before answering: the socket closes without `Registered`.
    let mut closed = false;
    while let Some(msg) = ws.next().await {
        match msg.expect("ws msg") {
            Ws::Close(_) => {
                closed = true;
                break;
            }
            Ws::Text(t) => {
                if let Ok(Frame::Registered) = serde_json::from_str::<Frame>(&t) {
                    panic!("an alias mismatch must never be registered");
                }
            }
            _ => continue,
        }
    }
    assert!(closed, "the mount closes on an alias mismatch");
}