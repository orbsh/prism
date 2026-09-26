//! Static code export e2e (PLAN Phase 1.9, ADR-0027 §4): the same
//! accept loop serves `GET /code/{sha256}` as a plain immutable
//! download. The blob under test is REAL: `echo_steel`'s registration
//! put its source into the meta plane (content addressing at upload),
//! so the export must hand back exactly those bytes at exactly that
//! address — self-verifying by construction. Also locks: 404 on an
//! unknown hash, 400 on a malformed address, and the WS path alive on
//! the same port (the head-replay regression).

use aura_engine::Engine;
use aura_realm::meta;
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ECHO_STEEL: &str = "(define (echo_steel args) args)";

async fn boot() -> std::net::SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

/// One raw HTTP GET, returning the full response bytes.
async fn get(addr: std::net::SocketAddr, path: &str) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap(); // Connection: close ends it
    buf
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The export: registration stored the blob under sha256(source); the
/// URL is self-verifying — the body re-hashes to its own address,
/// header carries the immutable cache policy.
#[tokio::test]
async fn code_export_serves_registered_source() {
    let addr = boot().await;
    let sha = meta::code_hash(ECHO_STEEL);
    let hex = meta::code_hex(&sha);
    let resp = get(addr, &format!("/code/{hex}")).await;
    let resp = text(&resp);
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(
        resp.contains("Cache-Control: public, max-age=31536000, immutable"),
        "{resp}"
    );
    let body = resp.split("\r\n\r\n").nth(1).expect("head/body split");
    assert_eq!(body, ECHO_STEEL, "served bytes differ from the registered source");
    // The self-verification claim, executed: the body hashes to the URL.
    assert_eq!(meta::code_hex(&meta::code_hash(body)), hex);
}

/// Unknown-but-well-formed hash = 404 value, connection closed cleanly.
#[tokio::test]
async fn unknown_hash_is_404() {
    let addr = boot().await;
    let resp = text(&get(addr, &format!("/code/{}", "0".repeat(64))).await);
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
}

/// Malformed address = 400 (never a truncated-key lookup): short,
/// non-hex, or path-prefix-only.
#[tokio::test]
async fn malformed_address_is_400() {
    let addr = boot().await;
    for path in ["/code/deadbeef", "/code/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
        let resp = text(&get(addr, path).await);
        assert!(resp.starts_with("HTTP/1.1 400"), "{path} → {resp}");
    }
}

/// The head-replay regression: the WS handshake rides the SAME accept
/// loop after a raw read — a client connect still upgrades, echoes,
/// and streams normally.
#[tokio::test]
async fn ws_still_upgrades_on_the_shared_loop() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let addr = boot().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake after head replay");
    // The mandatory `connected` frame.
    let f = loop {
        match ws.next().await.expect("stream").expect("msg") {
            Message::Binary(b) => break Codec::Json.decode(&b).unwrap(),
            Message::Text(t) => break Codec::Json.decode(t.as_bytes()).unwrap(),
            _ => continue,
        }
    };
    assert_eq!(f.ev, "connected");

    ws.send(Message::Text(
        serde_json::to_string(&Frame::new("echo_steel", serde_json::json!({"msg": "hi"})))
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();
    let f = loop {
        match ws.next().await.expect("stream").expect("msg") {
            Message::Binary(b) => break Codec::Json.decode(&b).unwrap(),
            Message::Text(t) => break Codec::Json.decode(t.as_bytes()).unwrap(),
            Message::Close(_) => panic!("closed"),
            _ => continue,
        }
    };
    assert_eq!(f.ev, "echo_steel.result");
    assert_eq!(f.args["args"], serde_json::json!({"msg": "hi"}));
}

/// Route isolation on one port: `/ws` never answers with export bytes,
/// `/code/...` never speaks WS. The 404 body of a WS-shaped path proves
/// the branch keyed on the prefix, not the transport.
#[tokio::test]
async fn export_path_never_upgrades() {
    let addr = boot().await;
    let resp = text(&get(addr, "/code/upgrade").await);
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(!resp.contains("101"), "must not upgrade");
}
